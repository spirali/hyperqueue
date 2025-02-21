use criterion::measurement::WallTime;
use criterion::{black_box, BatchSize, BenchmarkGroup, BenchmarkId, Criterion};
use tako::internal::server::core::Core;
use tako::ItemId;

use tako::Set;
use tako::TaskId;

use crate::{add_tasks, create_task};

fn bench_remove_single_task(c: &mut BenchmarkGroup<WallTime>) {
    for task_count in [10, 1_000, 100_000] {
        c.bench_with_input(
            BenchmarkId::new("remove a single task", task_count),
            &task_count,
            |b, &task_count| {
                b.iter_batched_ref(
                    || {
                        let mut core = Core::default();
                        add_tasks(&mut core, task_count);
                        (core, TaskId::new(0))
                    },
                    |(core, task_id)| {
                        let _ = core.remove_task(*task_id);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
}

fn bench_remove_all_tasks(c: &mut BenchmarkGroup<WallTime>) {
    for task_count in [10, 1_000, 100_000] {
        c.bench_with_input(
            BenchmarkId::new("remove all tasks", task_count),
            &task_count,
            |b, &task_count| {
                b.iter_batched_ref(
                    || {
                        let mut core = Core::default();
                        let tasks: Set<_> = add_tasks(&mut core, task_count).into_iter().collect();
                        (core, tasks)
                    },
                    |(core, tasks)| {
                        core.remove_tasks_batched(tasks);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
}

fn bench_add_task(c: &mut BenchmarkGroup<WallTime>) {
    for task_count in [10, 1_000, 100_000] {
        c.bench_with_input(
            BenchmarkId::new("add task", task_count),
            &task_count,
            |b, &task_count| {
                b.iter_batched_ref(
                    || {
                        let mut core = Core::default();
                        add_tasks(&mut core, task_count);

                        let task = create_task(TaskId::new(
                            (task_count + 1) as <TaskId as ItemId>::IdType,
                        ));
                        (core, Some(task))
                    },
                    |(core, task)| {
                        core.add_task(task.take().unwrap());
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
}

fn bench_add_tasks(c: &mut BenchmarkGroup<WallTime>) {
    for task_count in [10, 1_000, 100_000] {
        c.bench_with_input(
            BenchmarkId::new("add tasks", task_count),
            &task_count,
            |b, &task_count| {
                b.iter_batched_ref(
                    || {
                        let core = Core::default();
                        let tasks: Vec<_> = (0..task_count)
                            .map(|id| create_task(TaskId::new(id as <TaskId as ItemId>::IdType)))
                            .collect();
                        (core, tasks)
                    },
                    |(core, tasks)| {
                        for task in tasks.drain(..) {
                            core.add_task(task);
                        }
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
}

fn bench_iterate_tasks(c: &mut BenchmarkGroup<WallTime>) {
    for task_count in [10, 1_000, 100_000] {
        c.bench_with_input(
            BenchmarkId::new("iterate tasks", task_count),
            &task_count,
            |b, &task_count| {
                b.iter_batched_ref(
                    || {
                        let mut core = Core::default();
                        add_tasks(&mut core, task_count);
                        core
                    },
                    |ref mut core| {
                        let mut sum = 0;
                        for task in core.task_map().tasks() {
                            sum += task.id().as_num();
                        }
                        black_box(sum);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
}

pub fn benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("core");

    bench_remove_single_task(&mut group);
    bench_remove_all_tasks(&mut group);
    bench_add_task(&mut group);
    bench_add_tasks(&mut group);
    bench_iterate_tasks(&mut group);
}
