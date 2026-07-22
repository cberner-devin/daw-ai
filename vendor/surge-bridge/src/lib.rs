pub use surge_sys::*;

unsafe extern "C" {
    pub fn create_engine(sr: f32) -> *mut SurgeSynthesizer;
    pub fn create_patch() -> *mut SurgePatch;
    pub fn destroy_engine(surge: *mut SurgeSynthesizer);
    pub fn destroy_patch(patch: *mut SurgePatch);
    pub fn destroy_parameter(parameter: *mut Parameter);
}
