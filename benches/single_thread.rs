use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

fn enter_leave(c: &mut Criterion) {
    let mut group = c.benchmark_group("enter_leave");
    group.bench_function("seize", |b| {
        let collector = seize::Collector::new().unwrap();
        b.iter(|| {
            black_box(collector.enter().unwrap());
        });
    });

    group.bench_function("crossbeam", |b| {
        b.iter(|| {
            black_box(crossbeam_epoch::pin());
        });
    });
}

criterion_group!(benches, enter_leave);
criterion_main!(benches);
