use axial_resource::{
    PhysicalIoClass, PhysicalWorkError, PhysicalWorkRequest, process_physical_work,
};

pub(crate) async fn run<T, Work>(
    io: PhysicalIoClass,
    scratch_bytes: u64,
    work: Work,
) -> Result<T, PhysicalWorkError>
where
    T: Send + 'static,
    Work: FnOnce() -> T + Send + 'static,
{
    process_physical_work()
        .admit(PhysicalWorkRequest::foreground(io, scratch_bytes))
        .await?
        .run(move |_| work())
        .await
}
