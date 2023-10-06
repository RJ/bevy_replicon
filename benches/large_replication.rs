#[path = "../tests/common/mod.rs"]
mod common;

use std::{
    mem::size_of,
    time::{Duration, Instant},
};

use bevy::{app::MainScheduleOrder, ecs::schedule::ExecutorKind, prelude::*};
use bevy_replicon::prelude::*;
use criterion::{criterion_group, criterion_main, Criterion};
use serde::{Deserialize, Serialize};
use spin_sleep::{SpinSleeper, SpinStrategy};

#[derive(Component, Clone, Copy, Serialize, Deserialize)]
struct DummyComponent(usize, usize);

fn large_replication(c: &mut Criterion) {
    const ENTITIES: u32 = 5000;
    const SOCKET_WAIT: Duration = Duration::from_millis(5); // Sometimes it takes time for socket to receive all data.

    // Use spinner to keep CPU hot in the schedule for stable benchmark results.
    let sleeper = SpinSleeper::new(1_000_000_000).with_spin_strategy(SpinStrategy::SpinLoopHint);

    // update small frac of components
    fn update_components_system(
        tick: Res<RepliconTick>,
        mut q: Query<&mut DummyComponent, With<Replication>>,
    ) {
        for mut dc in q.iter_mut() {
            if (tick.get() + dc.0 as u32) % 100 == 0 {
                dc.1 += 1;
            }
        }
    }

    c.bench_function("send changing values", |b| {
        b.iter_custom(|iter| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iter {
                let mut server_app = App::new();
                let mut client_app = App::new();
                for app in [&mut server_app, &mut client_app] {
                    setup_app(app);
                }
                common::connect(&mut server_app, &mut client_app);
                server_app.add_systems(Update, update_components_system);

                // not measuring initial spawn on server
                for i in 0..ENTITIES {
                    server_app
                        .world
                        .spawn((Replication, DummyComponent(i as usize, 0)));
                }

                for _ in 0..1000 {
                    let instant = Instant::now();
                    server_app.update();
                    elapsed += instant.elapsed();

                    sleeper.sleep(SOCKET_WAIT);
                    client_app.update();
                    assert_eq!(client_app.world.entities().len(), ENTITIES);
                }
            }

            elapsed
        })
    });
}

fn setup_app(app: &mut App) {
    app.add_plugins((
        MinimalPlugins,
        ReplicationPlugins.set(ServerPlugin::new(TickPolicy::EveryFrame)),
    ))
    .replicate::<DummyComponent>();

    // TODO 0.12: Probably won't be needed since `multi-threaded` feature will be disabled by default.
    let labels = app.world.resource::<MainScheduleOrder>().labels.clone();
    for label in labels {
        app.edit_schedule(label, |schedule| {
            schedule.set_executor_kind(ExecutorKind::SingleThreaded);
        });
    }
}

criterion_group! {
    name = benches2;
    config = Criterion::default().sample_size(10);
    targets = large_replication
}
criterion_main!(benches2);
