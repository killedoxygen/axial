use axial_resource::{
    PhysicalIoClass, PhysicalWorkAdmission, PhysicalWorkError, PhysicalWorkParallelism,
    PhysicalWorkRequest, process_physical_work,
};

pub(crate) async fn admit(
    io: PhysicalIoClass,
    scratch_bytes: u64,
) -> Result<PhysicalWorkAdmission, PhysicalWorkError> {
    process_physical_work()
        .admit(PhysicalWorkRequest::foreground(io, scratch_bytes))
        .await
}

pub(crate) async fn run<T, Work>(
    io: PhysicalIoClass,
    scratch_bytes: u64,
    work: Work,
) -> Result<T, PhysicalWorkError>
where
    T: Send + 'static,
    Work: FnOnce() -> T + Send + 'static,
{
    admit(io, scratch_bytes).await?.run(move |_| work()).await
}

pub(crate) async fn run_parallel<T, Work>(
    io: PhysicalIoClass,
    scratch_bytes: u64,
    parallelism: usize,
    work: Work,
) -> Result<T, PhysicalWorkError>
where
    T: Send + 'static,
    Work: FnOnce(PhysicalWorkParallelism) -> T + Send + 'static,
{
    process_physical_work()
        .admit(PhysicalWorkRequest::foreground_parallel(
            io,
            scratch_bytes,
            parallelism,
        ))
        .await?
        .run_parallel(move |_, parallelism| work(parallelism))
        .await
}

pub(crate) async fn run_background<T, Work>(
    io: PhysicalIoClass,
    scratch_bytes: u64,
    work: Work,
) -> Result<T, PhysicalWorkError>
where
    T: Send + 'static,
    Work: FnOnce() -> T + Send + 'static,
{
    process_physical_work()
        .admit(PhysicalWorkRequest::background(io, scratch_bytes))
        .await?
        .run(move |_| work())
        .await
}
