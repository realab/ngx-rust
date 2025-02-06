use nginx_sys::ngx_module_t;
use std::ffi::c_void;

pub fn ngx_get_conf<T>(conf_ctx: *mut *mut *mut *mut c_void, module: *const ngx_module_t) -> *mut T {
    unsafe { (*conf_ctx).add((*module).index as usize) as *mut T }
}
