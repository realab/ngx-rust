use cpu_arl_rs::limiter;
use nginx_sys::ngx_http_log_handler_pt;
use std::ptr::{addr_of, addr_of_mut};

use once_cell::sync::Lazy;
use std::ffi::{c_char, c_void};
use std::sync::{Arc, RwLock};

use ngx::ffi::{
    ngx_array_push, ngx_command_t, ngx_conf_t, ngx_cycle_t, ngx_event_t, ngx_event_timer_rbtree, ngx_http_core_module,
    ngx_http_handler_pt, ngx_http_module_t, ngx_http_phases_NGX_HTTP_ACCESS_PHASE, ngx_http_phases_NGX_HTTP_LOG_PHASE,
    ngx_int_t, ngx_module_t, ngx_msec_int_t, ngx_msec_t, ngx_posted_events, ngx_process, ngx_queue_s,
    ngx_rbtree_delete, ngx_rbtree_insert, ngx_str_t, ngx_uint_t, NGX_CONF_TAKE1, NGX_HTTP_MAIN_CONF,
    NGX_HTTP_MAIN_CONF_OFFSET, NGX_HTTP_MODULE, NGX_PROCESS_WORKER, NGX_TIMER_LAZY_DELAY,
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

        // (*(*cf).cycle).

        start_background_task((*cf).cycle);

        core::Status::NGX_OK.into()
    }

    // unsafe extern "C" fn create_main_conf(cf: *mut ngx_conf_t) -> *mut c_void {
    //     start_background_task((*cf).cycle);
    //     let mut a: c_void = std::mem::zeroed();
    //     &mut a as *mut c_void
    // }
}

struct ModuleConfig {
    // limiter: limiter::ARLLimiter,
    enable: bool,
}

unsafe fn post_event(event: *mut ngx_event_t, queue: *mut ngx_queue_s) {
    let event = &mut (*event);
    if event.posted() == 0 {
        event.set_posted(1);
        // translated from ngx_queue_insert_tail macro
        event.queue.prev = (*queue).prev;
        (*event.queue.prev).next = &event.queue as *const _ as *mut _;
        event.queue.next = queue;
        (*queue).prev = &event.queue as *const _ as *mut _;
    }
}

static GLOBAL_LIMITER: Lazy<RwLock<Option<Arc<limiter::ARLLimiter>>>> = Lazy::new(|| RwLock::new(None));

impl Default for ModuleConfig {
    fn default() -> Self {
        // let rt = tokio::runtime::Builder::new_multi_thread()
        //     .enable_all()
        //     .build()
        //     .unwrap();
        // let opts = limiter::Options::default();
        // let handle = rt.spawn(async move {
        //     let limiter = limiter::ARLLimiter::new(opts);
        //     GLOBAL_LIMITER.write().unwrap().replace(Arc::new(limiter));
        // });
        // rt.block_on(handle).unwrap();
        Self {
            // limiter: GLOBAL_LIMITER.write().unwrap(),
            enable: false,
        }
    }
}

static mut MY_EVENT: ngx_event_t = unsafe { std::mem::zeroed() };

extern "C" fn background_task(ev: *mut ngx_event_t) {
    println!("background task: {:?}", std::time::Instant::now());
    // Re-schedule the task after 1000ms (1 second)
    ngx_event_add_timer(ev, 1000);
}

fn ngx_event_del_timer(ev: *mut ngx_event_t) {
    unsafe {
        ngx_rbtree_delete(&mut ngx_event_timer_rbtree as *mut _, &mut (*ev).timer as *mut _);

        // ngx_log_debug2(NGX_LOG_DEBUG_EVENT, ev->log, 0,
        //                "event timer del: %d: %M",
        //                 ngx_event_ident(ev->data), ev->timer.key);

        // ngx_rbtree_delete(&ngx_event_timer_rbtree, &ev->timer);

        (*ev).set_timer_set(0);
    }
}

fn ngx_event_add_timer(ev: *mut ngx_event_t, timer: ngx_msec_t) {
    unsafe {
        let key = ffi::ngx_current_msec + timer;

        println!("ngx_event_add_timer: key: {}/{}", key, (*ev).timer.key);

        if (*ev).timer_set() == 1 {
            let diff = (key - (*ev).timer.key) as ngx_msec_int_t;

            if diff.abs() < NGX_TIMER_LAZY_DELAY as isize {
                return;
            }

            ngx_event_del_timer(ev);
        }
        (*ev).timer.key = key;
        ngx_rbtree_insert(&raw mut ngx_event_timer_rbtree, &mut (*ev).timer);
        (*ev).set_timer_set(1);

        println!("ngx_event_add_timer: key: {}/{}", (*ev).timer.key, (*ev).timer_set());
    }
}

extern "C" fn start_background_task(cycle: *mut ngx_cycle_t) -> ngx_int_t {
    unsafe {
        if ngx_process != NGX_PROCESS_WORKER as usize {
            return core::Status::NGX_OK.into();
        }
    }

    println!("start background task");
    unsafe {
        MY_EVENT.handler = Some(background_task);
        MY_EVENT.set_cancelable(1);
        MY_EVENT.log = (*cycle).log;
        // MY_EVENT.set_timer_set(0);
        // MY_EVENT.set_active(1);

        // Start the timer, run the task every 1000ms (1 second)
        ngx_event_add_timer(&raw mut MY_EVENT, 1000);
        println!("start background task success");
        core::Status::NGX_OK.into()
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
    init_process: Some(start_background_task),
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
    unsafe {
        post_event(&raw mut MY_EVENT, addr_of_mut!(ngx_posted_events));
    }

    match co.enable {
        true => {
            // let bbr_ctx = request.pool().allocate::<NgxBBRCtx>(Default::default());
            // if bbr_ctx.is_null() {
            //     return core::Status::NGX_ERROR;
            // }
            // let limiter = GLOBAL_LIMITER.read().unwrap().as_ref().cloned().unwrap();
            // if let Ok(done) = limiter.allow() {
            //     ngx_log_debug_http!(request, "bbr module: allowed");
            //     unsafe {
            //         (*bbr_ctx).done = Some(done);
            //         request.set_module_ctx(bbr_ctx as *mut c_void, &*addr_of!(ngx_http_bbr_module));
            //     }
            //     return core::Status::NGX_DECLINED;
            // }
            // ngx_log_debug_http!(request, "bbr module: too many requests");
            // http::HTTPStatus::TOO_MANY_REQUESTS.into()

            return core::Status::NGX_DECLINED;
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
