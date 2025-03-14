// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use datafusion::common::tree_node::{TreeNode, VisitRecursion};
use datafusion::datasource::listing::{ListingTable, ListingTableUrl};
use datafusion::datasource::source_as_provider;
use datafusion::error::DataFusionError;
use std::any::type_name;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::scheduler_server::event::QueryStageSchedulerEvent;

use crate::state::executor_manager::{ExecutorManager, ExecutorReservation};
use crate::state::session_manager::SessionManager;
use crate::state::task_manager::{TaskLauncher, TaskManager};

use crate::cluster::BallistaCluster;
use crate::config::SchedulerConfig;
use crate::state::execution_graph::TaskDescription;
use ballista_core::error::{BallistaError, Result};
use ballista_core::serde::protobuf::TaskStatus;
use ballista_core::serde::BallistaCodec;
use datafusion::logical_expr::LogicalPlan;
use datafusion::physical_plan::display::DisplayableExecutionPlan;
use datafusion::physical_plan::ExecutionPlan;
use datafusion::prelude::SessionContext;
use datafusion_proto::logical_plan::AsLogicalPlan;
use datafusion_proto::physical_plan::AsExecutionPlan;
use log::{debug, error, info};
use prost::Message;

pub mod execution_graph;
pub mod execution_graph_dot;
pub mod executor_manager;
pub mod session_manager;
pub mod task_manager;

pub fn decode_protobuf<T: Message + Default>(bytes: &[u8]) -> Result<T> {
    T::decode(bytes).map_err(|e| {
        BallistaError::Internal(format!(
            "Could not deserialize {}: {}",
            type_name::<T>(),
            e
        ))
    })
}

pub fn decode_into<T: Message + Default + Into<U>, U>(bytes: &[u8]) -> Result<U> {
    T::decode(bytes)
        .map_err(|e| {
            BallistaError::Internal(format!(
                "Could not deserialize {}: {}",
                type_name::<T>(),
                e
            ))
        })
        .map(|t| t.into())
}

pub fn encode_protobuf<T: Message + Default>(msg: &T) -> Result<Vec<u8>> {
    let mut value: Vec<u8> = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut value).map_err(|e| {
        BallistaError::Internal(format!(
            "Could not serialize {}: {}",
            type_name::<T>(),
            e
        ))
    })?;
    Ok(value)
}

#[derive(Clone)]
pub struct SchedulerState<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan> {
    pub executor_manager: ExecutorManager,
    pub task_manager: TaskManager<T, U>,
    pub session_manager: SessionManager,
    pub codec: BallistaCodec<T, U>,
    pub config: Arc<SchedulerConfig>,
}

impl<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan> SchedulerState<T, U> {
    pub fn new(
        cluster: BallistaCluster,
        codec: BallistaCodec<T, U>,
        scheduler_name: String,
        config: Arc<SchedulerConfig>,
    ) -> Self {
        Self {
            executor_manager: ExecutorManager::new(
                cluster.cluster_state(),
                config.clone(),
            ),
            task_manager: TaskManager::new(
                cluster.job_state(),
                codec.clone(),
                scheduler_name,
            ),
            session_manager: SessionManager::new(cluster.job_state()),
            codec,
            config,
        }
    }

    #[cfg(test)]
    pub fn new_with_default_scheduler_name(
        cluster: BallistaCluster,
        codec: BallistaCodec<T, U>,
    ) -> Self {
        let config = Arc::new(SchedulerConfig::default());
        SchedulerState::new(cluster, codec, "localhost:50050".to_owned(), config)
    }

    #[allow(dead_code)]
    pub(crate) fn new_with_task_launcher(
        cluster: BallistaCluster,
        codec: BallistaCodec<T, U>,
        scheduler_name: String,
        config: Arc<SchedulerConfig>,
        dispatcher: Arc<dyn TaskLauncher>,
    ) -> Self {
        Self {
            executor_manager: ExecutorManager::new(
                cluster.cluster_state(),
                config.clone(),
            ),
            task_manager: TaskManager::with_launcher(
                cluster.job_state(),
                codec.clone(),
                scheduler_name,
                dispatcher,
            ),
            session_manager: SessionManager::new(cluster.job_state()),
            codec,
            config,
        }
    }

    pub async fn init(&self) -> Result<()> {
        self.executor_manager.init().await
    }

    pub(crate) async fn update_task_statuses(
        &self,
        executor_id: &str,
        tasks_status: Vec<TaskStatus>,
    ) -> Result<(Vec<QueryStageSchedulerEvent>, Vec<ExecutorReservation>)> {
        let executor = self
            .executor_manager
            .get_executor_metadata(executor_id)
            .await?;

        let total_num_tasks = tasks_status.len();
        let reservations = (0..total_num_tasks)
            .map(|_| ExecutorReservation::new_free(executor_id.to_owned()))
            .collect();

        let events = self
            .task_manager
            .update_task_statuses(&executor, tasks_status)
            .await?;

        Ok((events, reservations))
    }

    /// Process reservations which are offered. The basic process is
    /// 1. Attempt to fill the offered reservations with available tasks
    /// 2. For any reservation that filled, launch the assigned task on the executor.
    /// 3. For any reservations that could not be filled, cancel the reservation (i.e. return the
    ///    task slot back to the pool of available task slots).
    ///
    /// NOTE Error handling in this method is very important. No matter what we need to ensure
    /// that unfilled reservations are cancelled or else they could become permanently "invisible"
    /// to the scheduler.
    pub(crate) async fn offer_reservation(
        &self,
        reservations: Vec<ExecutorReservation>,
    ) -> Result<(Vec<ExecutorReservation>, usize)> {
        let pending_tasks = match self.task_manager.fill_reservations(&reservations).await
        {
            Ok((assignments, unassigned_reservations, pending_tasks)) => {
                let executor_stage_assignments = Self::combine_task(assignments);

                self.spawn_tasks_and_persist_reservations_back(
                    executor_stage_assignments,
                    unassigned_reservations,
                );

                pending_tasks
            }
            // If error set all reservations back
            Err(e) => {
                error!("Error filling reservations: {:?}", e);
                self.executor_manager
                    .cancel_reservations(reservations)
                    .await?;
                0
            }
        };

        let mut new_reservations = vec![];

        if pending_tasks > 0 {
            // If there are pending tasks available, try and schedule them
            let pending_reservations = self
                .executor_manager
                .reserve_slots(pending_tasks as u32)
                .await?;
            new_reservations.extend(pending_reservations);
        }

        Ok((new_reservations, pending_tasks))
    }

    fn spawn_tasks_and_persist_reservations_back(
        &self,
        executor_stage_assignments: HashMap<
            String,
            HashMap<(String, usize), Vec<TaskDescription>>,
        >,
        mut unassigned_reservations: Vec<ExecutorReservation>,
    ) {
        let task_manager = self.task_manager.clone();
        let executor_manager = self.executor_manager.clone();

        tokio::spawn(async move {
            for (executor_id, tasks) in executor_stage_assignments.into_iter() {
                let tasks: Vec<Vec<TaskDescription>> = tasks.into_values().collect();
                // Total number of tasks to be launched for one executor
                let n_tasks: usize =
                    tasks.iter().map(|stage_tasks| stage_tasks.len()).sum();

                match executor_manager.get_executor_metadata(&executor_id).await {
                    Ok(executor) => {
                        if let Err(e) = task_manager
                            .launch_multi_task(&executor, tasks, &executor_manager)
                            .await
                        {
                            error!("Failed to launch new task: {:?}", e);
                            // set resource back.
                            unassigned_reservations.append(&mut vec![
                                    ExecutorReservation::new_free(
                                        executor_id.clone(),
                                    );
                                    n_tasks
                                ]);
                        }
                    }
                    Err(e) => {
                        error!("Failed to launch new task, could not get executor metadata: {:?}", e);
                        // here no need set resource back.
                    }
                };
            }
            if !unassigned_reservations.is_empty() {
                // If any reserved slots remain, return them to the pool
                executor_manager
                    .cancel_reservations(unassigned_reservations)
                    .await
                    .expect("cancel_reservations fail!");
            }
        });
    }

    // Put tasks to the same executor together
    // And put tasks belonging to the same stage together for creating MultiTaskDefinition
    // return a map of <executor_id, <stage_key, TaskDesc>>.
    fn combine_task(
        assignments: Vec<(String, TaskDescription)>,
    ) -> HashMap<String, HashMap<(String, usize), Vec<TaskDescription>>> {
        let mut executor_stage_assignments: HashMap<
            String,
            HashMap<(String, usize), Vec<TaskDescription>>,
        > = HashMap::new();
        for (executor_id, task) in assignments.into_iter() {
            let stage_key = (task.partition.job_id.clone(), task.partition.stage_id);
            if let Some(tasks) = executor_stage_assignments.get_mut(&executor_id) {
                if let Some(executor_stage_tasks) = tasks.get_mut(&stage_key) {
                    executor_stage_tasks.push(task);
                } else {
                    tasks.insert(stage_key, vec![task]);
                }
            } else {
                let mut executor_stage_tasks: HashMap<
                    (String, usize),
                    Vec<TaskDescription>,
                > = HashMap::new();
                executor_stage_tasks.insert(stage_key, vec![task]);
                executor_stage_assignments.insert(executor_id, executor_stage_tasks);
            }
        }
        executor_stage_assignments
    }

    pub(crate) async fn plan_job(
        &self,
        job_id: &str,
        session_ctx: Arc<SessionContext>,
        plan: &LogicalPlan,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let start = Instant::now();

        if log::max_level() >= log::Level::Debug {
            // optimizing the plan here is redundant because the physical planner will do this again
            // but it is helpful to see what the optimized plan will be
            let optimized_plan = session_ctx.state().optimize(plan)?;
            debug!("Optimized plan: {}", optimized_plan.display_indent());
        }

        plan.apply(&mut |plan| {
            if let LogicalPlan::TableScan(scan) = plan {
                let provider = source_as_provider(&scan.source)?;
                if let Some(table) = provider.as_any().downcast_ref::<ListingTable>() {
                    let local_paths: Vec<&ListingTableUrl> = table
                        .table_paths()
                        .iter()
                        .filter(|url| url.as_str().starts_with("file:///"))
                        .collect();
                    if !local_paths.is_empty() {
                        // These are local files rather than remote object stores, so we
                        // need to check that they are accessible on the scheduler (the client
                        // may not be on the same host, or the data path may not be correctly
                        // mounted in the container). There could be thousands of files so we
                        // just check the first one.
                        let url = &local_paths[0].as_str();
                        // the unwraps are safe here because we checked that the url starts with file:///
                        // we need to check both versions here to support Linux & Windows
                        ListingTableUrl::parse(url.strip_prefix("file://").unwrap())
                            .or_else(|_| {
                                ListingTableUrl::parse(
                                    url.strip_prefix("file:///").unwrap(),
                                )
                            })
                            .map_err(|e| {
                                DataFusionError::External(
                                    format!(
                                    "logical plan refers to path on local file system \
                                that is not accessible in the scheduler: {url}: {e:?}"
                                )
                                    .into(),
                                )
                            })?;
                    }
                }
            }
            Ok(VisitRecursion::Continue)
        })?;

        let plan = session_ctx.state().create_physical_plan(plan).await?;
        debug!(
            "Physical plan: {}",
            DisplayableExecutionPlan::new(plan.as_ref()).indent()
        );

        let elapsed = start.elapsed();

        info!("Planned job {} in {:?}", job_id, elapsed);

        Ok(plan)
    }

    /// Spawn a delayed future to clean up job data on both Scheduler and Executors
    pub(crate) fn clean_up_successful_job(&self, job_id: String) {
        self.executor_manager.clean_up_job_data_delayed(
            job_id.clone(),
            self.config.finished_job_data_clean_up_interval_seconds,
        );
        self.task_manager.clean_up_job_delayed(
            job_id,
            self.config.finished_job_state_clean_up_interval_seconds,
        );
    }

    /// Spawn a delayed future to clean up job data on both Scheduler and Executors
    pub(crate) fn clean_up_failed_job(&self, job_id: String) {
        self.executor_manager.clean_up_job_data(job_id.clone());
        self.task_manager.clean_up_job_delayed(
            job_id,
            self.config.finished_job_state_clean_up_interval_seconds,
        );
    }
}

#[cfg(test)]
mod test {

    use crate::state::SchedulerState;
    use ballista_core::config::{BallistaConfig, BALLISTA_DEFAULT_SHUFFLE_PARTITIONS};
    use ballista_core::error::Result;
    use ballista_core::serde::protobuf::{
        task_status, ShuffleWritePartition, SuccessfulTask, TaskStatus,
    };
    use ballista_core::serde::scheduler::{
        ExecutorData, ExecutorMetadata, ExecutorSpecification,
    };
    use ballista_core::serde::BallistaCodec;

    use crate::config::SchedulerConfig;

    use crate::scheduler_server::timestamp_millis;
    use crate::test_utils::{test_cluster_context, BlackholeTaskLauncher};
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::logical_expr::{col, sum};
    use datafusion::physical_plan::ExecutionPlan;
    use datafusion::prelude::SessionContext;
    use datafusion::test_util::scan_empty;
    use datafusion_proto::protobuf::LogicalPlanNode;
    use datafusion_proto::protobuf::PhysicalPlanNode;
    use std::sync::Arc;

    const TEST_SCHEDULER_NAME: &str = "localhost:50050";

    // We should free any reservations which are not assigned
    #[tokio::test]
    async fn test_offer_free_reservations() -> Result<()> {
        let state: Arc<SchedulerState<LogicalPlanNode, PhysicalPlanNode>> =
            Arc::new(SchedulerState::new_with_default_scheduler_name(
                test_cluster_context(),
                BallistaCodec::default(),
            ));

        let executors = test_executors(1, 4);

        let (executor_metadata, executor_data) = executors[0].clone();

        let reservations = state
            .executor_manager
            .register_executor(executor_metadata, executor_data, true)
            .await?;

        let (result, assigned) = state.offer_reservation(reservations).await?;

        assert_eq!(assigned, 0);
        assert!(result.is_empty());

        // Need sleep wait for the spawn task work done.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        // All reservations should have been cancelled so we should be able to reserve them now
        let reservations = state.executor_manager.reserve_slots(4).await?;

        assert_eq!(reservations.len(), 4);

        Ok(())
    }

    // We should fill unbound reservations to any available task
    #[tokio::test]
    async fn test_offer_fill_reservations() -> Result<()> {
        let config = BallistaConfig::builder()
            .set(BALLISTA_DEFAULT_SHUFFLE_PARTITIONS, "4")
            .build()?;

        let scheduler_config = SchedulerConfig::default();
        let state: Arc<SchedulerState<LogicalPlanNode, PhysicalPlanNode>> =
            Arc::new(SchedulerState::new_with_task_launcher(
                test_cluster_context(),
                BallistaCodec::default(),
                TEST_SCHEDULER_NAME.into(),
                Arc::new(scheduler_config),
                Arc::new(BlackholeTaskLauncher::default()),
            ));

        let session_ctx = state.session_manager.create_session(&config).await?;

        let plan = test_graph(session_ctx.clone()).await;

        // Create 4 jobs so we have four pending tasks
        state
            .task_manager
            .queue_job("job-1", "", timestamp_millis())
            .await?;
        state
            .task_manager
            .submit_job(
                "job-1",
                "",
                session_ctx.session_id().as_str(),
                plan.clone(),
                0,
            )
            .await?;
        state
            .task_manager
            .queue_job("job-2", "", timestamp_millis())
            .await?;
        state
            .task_manager
            .submit_job(
                "job-2",
                "",
                session_ctx.session_id().as_str(),
                plan.clone(),
                0,
            )
            .await?;
        state
            .task_manager
            .queue_job("job-3", "", timestamp_millis())
            .await?;
        state
            .task_manager
            .submit_job(
                "job-3",
                "",
                session_ctx.session_id().as_str(),
                plan.clone(),
                0,
            )
            .await?;
        state
            .task_manager
            .queue_job("job-4", "", timestamp_millis())
            .await?;
        state
            .task_manager
            .submit_job(
                "job-4",
                "",
                session_ctx.session_id().as_str(),
                plan.clone(),
                0,
            )
            .await?;

        let executors = test_executors(1, 4);

        let (executor_metadata, executor_data) = executors[0].clone();

        let reservations = state
            .executor_manager
            .register_executor(executor_metadata, executor_data, true)
            .await?;

        let (result, pending) = state.offer_reservation(reservations).await?;

        assert_eq!(pending, 0);
        assert!(result.is_empty());

        // All task slots should be assigned so we should not be able to reserve more tasks
        let reservations = state.executor_manager.reserve_slots(4).await?;

        assert_eq!(reservations.len(), 0);

        Ok(())
    }

    // We should generate a new event for tasks that are still pending
    #[tokio::test]
    async fn test_offer_resubmit_pending() -> Result<()> {
        let config = BallistaConfig::builder()
            .set(BALLISTA_DEFAULT_SHUFFLE_PARTITIONS, "4")
            .build()?;

        let scheduler_config = SchedulerConfig::default();
        let state: Arc<SchedulerState<LogicalPlanNode, PhysicalPlanNode>> =
            Arc::new(SchedulerState::new_with_task_launcher(
                test_cluster_context(),
                BallistaCodec::default(),
                TEST_SCHEDULER_NAME.into(),
                Arc::new(scheduler_config),
                Arc::new(BlackholeTaskLauncher::default()),
            ));

        let session_ctx = state.session_manager.create_session(&config).await?;

        let plan = test_graph(session_ctx.clone()).await;

        // Create a job
        state
            .task_manager
            .queue_job("job-1", "", timestamp_millis())
            .await?;
        state
            .task_manager
            .submit_job(
                "job-1",
                "",
                session_ctx.session_id().as_str(),
                plan.clone(),
                0,
            )
            .await?;

        let executors = test_executors(1, 4);

        let (executor_metadata, executor_data) = executors[0].clone();

        // Complete the first stage. So we should now have 4 pending tasks for this job stage 2
        {
            let plan_graph = state
                .task_manager
                .get_active_execution_graph("job-1")
                .unwrap();
            let task_def = plan_graph
                .write()
                .await
                .pop_next_task(&executor_data.executor_id)?
                .unwrap();
            let mut partitions: Vec<ShuffleWritePartition> = vec![];
            for partition_id in 0..4 {
                partitions.push(ShuffleWritePartition {
                    partition_id: partition_id as u64,
                    path: "some/path".to_string(),
                    num_batches: 1,
                    num_rows: 1,
                    num_bytes: 1,
                })
            }
            state
                .task_manager
                .update_task_statuses(
                    &executor_metadata,
                    vec![TaskStatus {
                        task_id: task_def.task_id as u32,
                        job_id: "job-1".to_string(),
                        stage_id: task_def.partition.stage_id as u32,
                        stage_attempt_num: task_def.stage_attempt_num as u32,
                        partition_id: task_def.partition.partition_id as u32,
                        launch_time: 0,
                        start_exec_time: 0,
                        end_exec_time: 0,
                        metrics: vec![],
                        status: Some(task_status::Status::Successful(SuccessfulTask {
                            executor_id: executor_data.executor_id.clone(),
                            partitions,
                        })),
                    }],
                )
                .await?;
        }

        state
            .executor_manager
            .register_executor(executor_metadata, executor_data, false)
            .await?;

        let reservations = state.executor_manager.reserve_slots(1).await?;

        assert_eq!(reservations.len(), 1);

        // Offer the reservation. It should be filled with one of the 4 pending tasks. The other 3 should
        // be reserved for the other 3 tasks, emitting another offer event
        let (reservations, pending) = state.offer_reservation(reservations).await?;

        assert_eq!(pending, 3);
        assert_eq!(reservations.len(), 3);

        // Remaining 3 task slots should be reserved for pending tasks
        let reservations = state.executor_manager.reserve_slots(4).await?;

        assert_eq!(reservations.len(), 0);

        Ok(())
    }

    fn test_executors(
        total_executors: usize,
        slots_per_executor: u32,
    ) -> Vec<(ExecutorMetadata, ExecutorData)> {
        let mut result: Vec<(ExecutorMetadata, ExecutorData)> = vec![];

        for i in 0..total_executors {
            result.push((
                ExecutorMetadata {
                    id: format!("executor-{i}"),
                    host: format!("host-{i}"),
                    port: 8080,
                    grpc_port: 9090,
                    specification: ExecutorSpecification {
                        task_slots: slots_per_executor,
                    },
                },
                ExecutorData {
                    executor_id: format!("executor-{i}"),
                    total_task_slots: slots_per_executor,
                    available_task_slots: slots_per_executor,
                },
            ));
        }

        result
    }

    async fn test_graph(ctx: Arc<SessionContext>) -> Arc<dyn ExecutionPlan> {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("gmv", DataType::UInt64, false),
        ]);

        let plan = scan_empty(None, &schema, Some(vec![0, 1]))
            .unwrap()
            .aggregate(vec![col("id")], vec![sum(col("gmv"))])
            .unwrap()
            .build()
            .unwrap();

        ctx.state().create_physical_plan(&plan).await.unwrap()
    }
}
