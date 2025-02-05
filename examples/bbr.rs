use cpu_arl_rs::{cpu, limiter};
use nginx_sys::ngx_http_log_handler_pt;
use std::panic;
use std::ptr::addr_of;

use once_cell::sync::Lazy;
use std::ffi::{c_char, c_void};
use std::sync::{Arc, RwLock};

use ngx::ffi::{
    ngx_array_push, ngx_command_t, ngx_conf_t, ngx_connection_t, ngx_cycle_t, ngx_event_t, ngx_exiting,
    ngx_http_conf_ctx_t, ngx_http_core_module, ngx_http_handler_pt, ngx_http_module_t,
    ngx_http_phases_NGX_HTTP_ACCESS_PHASE, ngx_http_phases_NGX_HTTP_LOG_PHASE, ngx_int_t, ngx_module_t, ngx_process,
    ngx_quit, ngx_str_t, ngx_uint_t, ngx_worker, NGX_CONF_TAKE1, NGX_HTTP_MAIN_CONF, NGX_HTTP_MAIN_CONF_OFFSET,
    NGX_HTTP_MODULE, NGX_PROCESS_WORKER,
};
use ngx::http::{self, HTTPModule, MergeConfigError};
use ngx::{core, ffi};
use ngx::{http_log_handler, http_request_handler, ngx_log_error, ngx_null_command, ngx_string};

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

#[derive(Debug)]
struct ModuleConfig {
    cpu_provider: limiter::CPUStatProviderName,
    enable: bool,
}

static GLOBAL_CPU_LOADER: Lazy<Arc<RwLock<Option<Arc<cpu::EMACPUUsageLoader>>>>> =
    Lazy::new(|| Arc::new(RwLock::new(None)));

impl Default for ModuleConfig {
    fn default() -> Self {
        let cfg = Self {
            cpu_provider: limiter::CPUStatProviderName::Machine,
            enable: false,
        };

        cfg
    }
}

static mut NGX_HTTP_BBR_COMMANDS: [ngx_command_t; 2] = [
    ngx_command_t {
        name: ngx_string!("bbr"),
        type_: (NGX_HTTP_MAIN_CONF | NGX_CONF_TAKE1) as ngx_uint_t,
        set: Some(ngx_http_bbr_commands_set_enable),
        conf: NGX_HTTP_MAIN_CONF_OFFSET,
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
    init_process: Some(ngx_http_cpu_loader_init_process),
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
        let bbr_ctx = unsafe { request.get_mutable_module_ctx::<NgxBBRCtx>(&*addr_of!(ngx_http_bbr_module)) };
        if let Some(ctx) = bbr_ctx {
            // ngx_log_debug_http!(request, "bbr: found context",);
            ctx.call_done();
        };
        std::ptr::null_mut()
    }
);

static GLOBAL_LIMITER: Lazy<RwLock<Option<Arc<limiter::ARLLimiter>>>> = Lazy::new(|| RwLock::new(None));

http_request_handler!(bbr_access_handler, |request: &mut http::Request| {
    let co = unsafe { request.get_module_main_conf::<ModuleConfig>(&*addr_of!(ngx_http_bbr_module)) };
    let co = co.expect("module config is none");

    match co.enable {
        true => {
            // ngx_log_debug_http!(request, "bbr module enabled: {}", co.enable);

            let bbr_ctx = request.pool().allocate::<NgxBBRCtx>(Default::default());
            if bbr_ctx.is_null() {
                return core::Status::NGX_ERROR;
            }

            let limiter = GLOBAL_LIMITER.read().unwrap().as_ref().cloned().unwrap();
            if let Ok(done) = limiter.allow() {
                // ngx_log_debug_http!(request, "bbr module: allowed");
                unsafe {
                    (*bbr_ctx).done = Some(done);
                    request.set_module_ctx(bbr_ctx as *mut c_void, &*addr_of!(ngx_http_bbr_module));
                }
                return core::Status::NGX_DECLINED;
            }
            // ngx_log_debug_http!(request, "bbr module: BANDWIDTH_LIMIT_EXCEEDED");
            println!("bbr module: BANDWIDTH_LIMIT_EXCEEDED");
            http::HTTPStatus::BANDWIDTH_LIMIT_EXCEEDED.into()
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

#[no_mangle]
extern "C" fn ngx_http_cpu_loader_init_process(cycle: *mut ngx_cycle_t) -> ngx_int_t {
    unsafe {
        if ngx_process != NGX_PROCESS_WORKER as usize {
            return core::Status::NGX_OK.into();
        }

        ngx_log_error!(
            ffi::NGX_LOG_NOTICE,
            (*cycle).log,
            "[cron-module] Initializing cron timer in worker process {}",
            ngx_worker,
        );

        let bbr_cfg = {
            let http_ctx = (*cycle).conf_ctx.add(ngx_http_core_module.ctx_index as usize) as *mut ngx_http_conf_ctx_t;
            let raw_conf = (*http_ctx).main_conf.add(ngx_http_bbr_module.ctx_index) as *mut *mut ModuleConfig;
            unsafe { raw_conf.cast::<ModuleConfig>().as_ref().expect("module config is none") }
        };
        println!("bbr module enabled: {:?}/{:?}", bbr_cfg, std::process::id());
        match bbr_cfg.cpu_provider {
            limiter::CPUStatProviderName::Machine => {
                let loader = Arc::new(cpu::EMACPUUsageLoader::new(Box::new(
                    cpu::MachineCPUStatProvider::new().unwrap(),
                )));
                unsafe { GLOBAL_CPU_LOADER.write().unwrap().replace(loader) };
            }
            #[cfg(target_os = "linux")]
            limiter::CPUStatProviderName::CGroup => {
                use cpu_arl_rs::cgroup;
                let provider =
                    cgroup::CGroupCPUStatProvider::new(path::PathBuf::from("/sys/fs/cgroup/"), false).unwrap();
                let loader = Arc::new(cpu::EMACPUUsageLoader::new(Box::new(provider)));
                GLOBAL_CPU_LOADER.write().unwrap().replace(loader);
            }
            _ => {
                panic!("unsupported cpu provider: {:?}", bbr_cfg.cpu_provider);
            }
        }

        let opts = limiter::Options::default();
        let cpu_getter = Box::new(|| unsafe { GLOBAL_CPU_LOADER.read().unwrap().as_ref().unwrap().get_cpu_usage() });
        let limiter = limiter::ARLLimiter::new(cpu_getter, opts);
        GLOBAL_LIMITER.write().unwrap().replace(Arc::new(limiter));

        let ngx_http_cron_dummy_conn = core::Pool::from_ngx_pool((*cycle).pool)
            .alloc(std::mem::size_of::<ngx_connection_t>())
            as *mut ngx_connection_t;
        (*ngx_http_cron_dummy_conn).fd = -1;
        (*ngx_http_cron_dummy_conn).log = (*cycle).log;

        let ngx_http_core_timer =
            core::Pool::from_ngx_pool((*cycle).pool).alloc(std::mem::size_of::<ngx_event_t>()) as *mut ngx_event_t;
        (*ngx_http_core_timer).handler = Some(ngx_http_cpu_loader_timer_handler);
        (*ngx_http_core_timer).data = ngx_http_cron_dummy_conn as *mut c_void;
        (*ngx_http_core_timer).log = (*cycle).log;
        (*ngx_http_core_timer).set_cancelable(1);

        let timer: &mut core::Event = ngx_http_core_timer.into();
        timer.add_timer(1000);

        return core::Status::NGX_OK.into();
    }
}

#[no_mangle]
extern "C" fn ngx_http_cpu_loader_timer_handler(ev: *mut ngx_event_t) {
    GLOBAL_CPU_LOADER.read().unwrap().as_ref().unwrap().refresh_cpu_usage();

    let limiter = GLOBAL_LIMITER.read().unwrap();
    let limiter = limiter.as_ref().unwrap();
    unsafe {
        ngx_log_error!(
            ffi::NGX_LOG_ERR,
            (*ev).log,
            "[bbr-module] CPU usage: {:.2}%, max_inflight: {:?}, inflight: {:?}",
            GLOBAL_CPU_LOADER.read().unwrap().as_ref().unwrap().get_cpu_usage() / 10.0,
            limiter.max_in_flight(),
            limiter.in_flight(),
        );

        if !(ngx_exiting == 1) && !(ngx_quit == 1) {
            let event: &mut core::Event = ev.into();
            event.add_timer(1000);
        }
    }
}
