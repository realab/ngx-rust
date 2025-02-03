use cpu_arl_rs::limiter;
use nginx_sys::ngx_http_log_handler_pt;

use std::ffi::{c_char, c_void};
use std::io::empty;
use std::ptr::addr_of;

use ngx::ffi::{
    ngx_array_push, ngx_command_t, ngx_conf_t, ngx_http_core_module, ngx_http_handler_pt, ngx_http_module_t,
    ngx_http_phases_NGX_HTTP_ACCESS_PHASE, ngx_http_phases_NGX_HTTP_LOG_PHASE, ngx_int_t, ngx_module_t, ngx_str_t,
    ngx_uint_t, NGX_CONF_TAKE1, NGX_HTTP_LOC_CONF, NGX_HTTP_LOC_CONF_OFFSET, NGX_HTTP_MODULE,
};
use ngx::http::{self, HTTPModule, MergeConfigError};
use ngx::{core, ffi};
use ngx::{http_log_handler, http_request_handler, ngx_log_debug_http, ngx_null_command, ngx_string};

struct Module;

impl http::HTTPModule for Module {
    type MainConf = ModuleConfig;
    type SrvConf = ();
    type LocConf = ();

    unsafe extern "C" fn postconfiguration(cf: *mut ngx_conf_t) -> ngx_int_t {
        let cmcf = http::ngx_http_conf_get_module_main_conf(cf, &*addr_of!(ngx_http_core_module));

        let h = ngx_array_push(&mut (*cmcf).phases[ngx_http_phases_NGX_HTTP_ACCESS_PHASE as usize].handlers)
            as *mut ngx_http_handler_pt;
        if h.is_null() {
            return core::Status::NGX_ERROR.into();
        }
        // set an Access phase handler
        *h = Some(bbr_access_handler);

        let done_h = ngx_array_push(&mut (*cmcf).phases[ngx_http_phases_NGX_HTTP_LOG_PHASE as usize].handlers)
            as *mut ngx_http_log_handler_pt;
        if h.is_null() {
            return core::Status::NGX_ERROR.into();
        }
        *done_h = Some(bbr_done_handler);

        core::Status::NGX_OK.into()
    }
}

struct ModuleConfig {
    limiter: limiter::ARLLimiter,
    enable: bool,
}

impl Default for ModuleConfig {
    fn default() -> Self {
        Self {
            limiter: limiter::ARLLimiter::new(limiter::Options::default()),
            enable: false,
        }
    }
}

static mut NGX_HTTP_BBR_COMMANDS: [ngx_command_t; 2] = [
    ngx_command_t {
        name: ngx_string!("bbr"),
        type_: (NGX_HTTP_LOC_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(ngx_http_bbr_commands_set_enable),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: 0,
        post: std::ptr::null_mut(),
    },
    ngx_null_command!(),
];

static NGX_HTTP_BBR_MODULE_CTX: ngx_http_module_t = ngx_http_module_t {
    preconfiguration: Some(Module::preconfiguration),
    postconfiguration: Some(Module::postconfiguration),
    create_main_conf: Some(Module::create_main_conf),
    init_main_conf: Some(Module::init_main_conf),
    create_srv_conf: Some(Module::create_srv_conf),
    merge_srv_conf: Some(Module::merge_srv_conf),
    create_loc_conf: Some(Module::create_loc_conf),
    merge_loc_conf: Some(Module::merge_loc_conf),
};

// Generate the `ngx_modules` table with exported modules.
// This feature is required to build a 'cdylib' dynamic module outside of the NGINX buildsystem.
#[cfg(feature = "export-modules")]
ngx::ngx_modules!(ngx_http_bbr_module);

#[used]
#[allow(non_upper_case_globals)]
#[cfg_attr(not(feature = "export-modules"), no_mangle)]
pub static mut ngx_http_bbr_module: ngx_module_t = ngx_module_t {
    ctx: std::ptr::addr_of!(NGX_HTTP_BBR_MODULE_CTX) as _,
    commands: unsafe { &NGX_HTTP_BBR_COMMANDS[0] as *const _ as *mut _ },
    type_: NGX_HTTP_MODULE as _,
    ..ngx_module_t::default()
};

impl http::Merge for ModuleConfig {
    fn merge(&mut self, prev: &ModuleConfig) -> Result<(), MergeConfigError> {
        if prev.enable {
            self.enable = true;
        };
        Ok(())
    }
}

struct NgxBBRCtx {
    done: Option<Box<dyn FnOnce()>>,
}

impl Default for NgxBBRCtx {
    fn default() -> Self {
        Self { done: None }
    }
}

impl NgxBBRCtx {
    fn call_done(&mut self) {
        if let Some(done) = self.done.take() {
            done();
        }
    }
}

http_log_handler!(
    bbr_done_handler,
    |request: &mut http::Request, _: &mut http::Request| {
        use ffi;
        let bbr_ctx = unsafe { request.get_mutable_module_ctx::<NgxBBRCtx>(&*addr_of!(ngx_http_bbr_module)) };
        if let Some(ctx) = bbr_ctx {
            ngx_log_debug_http!(request, "bbr: found context",);
            ctx.call_done();
        };
        let ret = 0u8;
        &ret as *const _ as *mut ffi::u_char
    }
);

http_request_handler!(bbr_access_handler, |request: &mut http::Request| {
    let co = unsafe { request.get_module_main_conf::<ModuleConfig>(&*addr_of!(ngx_http_bbr_module)) };
    let co = co.expect("module config is none");

    ngx_log_debug_http!(request, "bbr module enabled: {}", co.enable);

    match co.enable {
        true => {
            let bbr_ctx = request.pool().allocate::<NgxBBRCtx>(Default::default());
            if bbr_ctx.is_null() {
                return core::Status::NGX_ERROR;
            }
            if let Ok(done) = co.limiter.allow() {
                unsafe {
                    (*bbr_ctx).done = Some(Box::new(done));
                    request.set_module_ctx(bbr_ctx as *mut c_void, &*addr_of!(ngx_http_bbr_module));
                }
                return core::Status::NGX_DECLINED;
            }
            ngx_log_debug_http!(request, "bbr module: too many requests");
            http::HTTPStatus::TOO_MANY_REQUESTS.into()
        }
        false => core::Status::NGX_DECLINED,
    }
});

extern "C" fn ngx_http_bbr_commands_set_enable(
    cf: *mut ngx_conf_t,
    _cmd: *mut ngx_command_t,
    conf: *mut c_void,
) -> *mut c_char {
    unsafe {
        let conf = &mut *(conf as *mut ModuleConfig);
        let args = (*(*cf).args).elts as *mut ngx_str_t;

        let val = (*args.add(1)).to_str();

        // set default value optionally
        conf.enable = false;

        if val.len() == 2 && val.eq_ignore_ascii_case("on") {
            conf.enable = true;
        } else if val.len() == 3 && val.eq_ignore_ascii_case("off") {
            conf.enable = false;
        }
    };

    std::ptr::null_mut()
}
