use cpu_arl_rs::limiter;
use nginx_sys::ngx_http_log_handler_pt;
use ngx::core::Event;
use std::ptr::{addr_of, addr_of_mut};

use once_cell::sync::Lazy;
use std::ffi::{c_char, c_void};
use std::sync::{Arc, RwLock};

use ngx::ffi::{
    ngx_array_push, ngx_command_t, ngx_conf_t, ngx_connection_t, ngx_cycle_t, ngx_event_t, ngx_event_timer_rbtree,
    ngx_exiting, ngx_http_core_module, ngx_http_handler_pt, ngx_http_module_t, ngx_http_phases_NGX_HTTP_ACCESS_PHASE,
    ngx_http_phases_NGX_HTTP_LOG_PHASE, ngx_int_t, ngx_module_t, ngx_msec_int_t, ngx_msec_t, ngx_posted_events,
    ngx_process, ngx_queue_s, ngx_quit, ngx_rbtree_delete, ngx_rbtree_insert, ngx_str_t, ngx_uint_t, ngx_worker,
    NGX_CONF_TAKE1, NGX_HTTP_MAIN_CONF, NGX_HTTP_MAIN_CONF_OFFSET, NGX_HTTP_MODULE, NGX_PROCESS_WORKER,
    NGX_TIMER_LAZY_DELAY,
};
use ngx::http::{self, HTTPModule, MergeConfigError};
use ngx::{core, ffi};
use ngx::{http_log_handler, http_request_handler, ngx_log_debug_http, ngx_log_error, ngx_null_command, ngx_string};

struct TaskModule;

impl http::HTTPModule for TaskModule {
    type MainConf = ();
    type SrvConf = ();
    type LocConf = ();
}

static mut NGX_HTTP_TASK_COMMANDS: [ngx_command_t; 1] = [ngx_null_command!()];

static NGX_HTTP_TASK_MODULE_CTX: ngx_http_module_t = ngx_http_module_t {
    preconfiguration: Some(TaskModule::preconfiguration),
    postconfiguration: Some(TaskModule::postconfiguration),
    create_main_conf: Some(TaskModule::create_main_conf),
    init_main_conf: Some(TaskModule::init_main_conf),
    create_srv_conf: Some(TaskModule::create_srv_conf),
    merge_srv_conf: Some(TaskModule::merge_srv_conf),
    create_loc_conf: Some(TaskModule::create_loc_conf),
    merge_loc_conf: Some(TaskModule::merge_loc_conf),
};

// Generate the `ngx_modules` table with exported modules.
// This feature is required to build a 'cdylib' dynamic module outside of the NGINX buildsystem.
#[cfg(feature = "export-modules")]
ngx::ngx_modules!(ngx_http_task_module);

#[used]
#[allow(non_upper_case_globals)]
#[cfg_attr(not(feature = "export-modules"), no_mangle)]
pub static mut ngx_http_task_module: ngx_module_t = ngx_module_t {
    ctx: std::ptr::addr_of!(NGX_HTTP_TASK_MODULE_CTX) as _,
    commands: unsafe { &NGX_HTTP_TASK_COMMANDS[0] as *const _ as *mut _ },
    type_: NGX_HTTP_MODULE as _,
    init_process: Some(ngx_http_cron_init_process),
    ..ngx_module_t::default()
};

// static ngx_int_t
// ngx_http_cron_init_process(ngx_cycle_t *cycle)
// {
//     // Only run in actual worker processes (not master or cache loader processes).
//     if (ngx_process != NGX_PROCESS_WORKER) {
//         return NGX_OK;
//     }

//     ngx_log_error(NGX_LOG_NOTICE, cycle->log, 0,
//                   "[cron-module] Initializing cron timer in worker process %d",
//                   ngx_worker);

//     // Initialize a dummy connection for the timer event
//     ngx_memzero(&ngx_http_cron_dummy_conn, sizeof(ngx_http_cron_dummy_conn));
//     ngx_http_cron_dummy_conn.fd = (ngx_socket_t) -1;
//     ngx_http_cron_dummy_conn.log = cycle->log;

//     // Set up the event
//     ngx_memzero(&ngx_http_cron_timer, sizeof(ngx_http_cron_timer));
//     ngx_http_cron_timer.handler = ngx_http_cron_timer_handler;
//     ngx_http_cron_timer.data    = NULL;
//     ngx_http_cron_timer.log     = cycle->log;
//     ngx_http_cron_timer.cancelable = 1;

//     // Schedule the first timer event
//     ngx_add_timer(&ngx_http_cron_timer, NGX_HTTP_CRON_INTERVAL);

//     return NGX_OK;
// }

#[no_mangle]
extern "C" fn ngx_http_cron_init_process(cycle: *mut ngx_cycle_t) -> ngx_int_t {
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

        let mut ngx_http_cron_dummy_conn: ngx_connection_t = std::mem::zeroed();
        ngx_http_cron_dummy_conn.fd = -1;
        ngx_http_cron_dummy_conn.log = (*cycle).log;

        let ngx_http_core_timer =
            core::Pool::from_ngx_pool((*cycle).pool).alloc(std::mem::size_of::<ngx_event_t>()) as *mut ngx_event_t;
        (*ngx_http_core_timer).handler = Some(ngx_http_cron_timer_handler);
        (*ngx_http_core_timer).data = std::ptr::null_mut();
        (*ngx_http_core_timer).log = (*cycle).log;
        (*ngx_http_core_timer).set_cancelable(1);

        let timer: &mut Event = ngx_http_core_timer.into();
        timer.add_timer(10000);

        return core::Status::NGX_OK.into();
    }
}

#[no_mangle]
extern "C" fn ngx_http_cron_timer_handler(ev: *mut ngx_event_t) {
    unsafe {
        ngx_log_error!(
            ffi::NGX_LOG_NOTICE,
            (*ev).log,
            "[cron-module] Timer triggered. Do your periodic work here."
        );

        if !(ngx_exiting == 1) && !(ngx_quit == 1) {
            let event: &mut Event = ev.into();
            event.add_timer(10000);
        }
    }
}
