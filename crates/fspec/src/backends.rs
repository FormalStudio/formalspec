use fspec_core::Backend;
use fspec_files::FileBackend;
use fspec_image::ImageBackend;
use fspec_os::{
    DefaultsBackend, DinitBackend, LaunchdBackend, LocaleBackend, OpenrcBackend, RunitBackend, S6Backend,
    SettingBackend, SystemdBackend, UsersBackend,
};
use fspec_pkgmgr::{
    AptBackend, BrewBackend, DistroBackend, DnfBackend, FlatpakBackend, MossBackend, PacmanBackend, XbpsBackend,
    ZypperBackend,
};
use fspec_store::StoreBackend;

/// Every backend this build knows. Each claims the resources it can apply on a given host.
pub fn all() -> Vec<&'static dyn Backend> {
    vec![
        &UsersBackend,
        &FileBackend,
        &StoreBackend,
        &FlatpakBackend,
        &BrewBackend,
        &DnfBackend,
        &PacmanBackend,
        &AptBackend,
        &XbpsBackend,
        &MossBackend,
        &ZypperBackend,
        &DistroBackend,
        &SystemdBackend,
        &RunitBackend,
        &OpenrcBackend,
        &DinitBackend,
        &S6Backend,
        &LaunchdBackend,
        &DefaultsBackend,
        &SettingBackend,
        &LocaleBackend,
        &ImageBackend,
    ]
}
