use ngx::core::Event;
use ngx::ffi::{
    ngx_command_t, ngx_connection_t, ngx_cycle_t, ngx_event_t, ngx_exiting, ngx_http_module_t, ngx_int_t, ngx_module_t,
    ngx_process, ngx_quit, ngx_worker, NGX_HTTP_MODULE, NGX_PROCESS_WORKER,
};
use ngx::http::{self, HTTPModule};
use ngx::{core, ffi};
use ngx::{ngx_log_error, ngx_null_command};
use std::ffi::c_void;

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

#[no_mangle]
extern "C" fn ngx_http_cron_init_process(cycle: *mut ngx_cycle_t) -> ngx_int_t {
    unsafe {
        if ngx_process != NGX_PROCESS_WORKER as usize {
            return core::Status::NGX_OK.into();
        }

        ngx_hyper::handle_request(cycle);

        ngx_log_error!(
            ffi::NGX_LOG_NOTICE,
            (*cycle).log,
            "[cron-module] Initializing cron timer in worker process {}",
            ngx_worker,
        );

        let ngx_http_cron_dummy_conn = core::Pool::from_ngx_pool((*cycle).pool)
            .alloc(std::mem::size_of::<ngx_connection_t>())
            as *mut ngx_connection_t;
        (*ngx_http_cron_dummy_conn).fd = -1;
        (*ngx_http_cron_dummy_conn).log = (*cycle).log;

        let ngx_http_core_timer =
            core::Pool::from_ngx_pool((*cycle).pool).alloc(std::mem::size_of::<ngx_event_t>()) as *mut ngx_event_t;
        (*ngx_http_core_timer).handler = Some(ngx_http_cron_timer_handler);
        (*ngx_http_core_timer).data = ngx_http_cron_dummy_conn as *mut c_void;
        (*ngx_http_core_timer).log = (*cycle).log;
        (*ngx_http_core_timer).set_cancelable(1);

        let timer: &mut Event = ngx_http_core_timer.into();
        timer.add_timer(1000);

        return core::Status::NGX_OK.into();
    }
}

#[no_mangle]
extern "C" fn ngx_http_cron_timer_handler(ev: *mut ngx_event_t) {
    unsafe {
        ngx_log_error!(
            ffi::NGX_LOG_NOTICE,
            (*ev).log,
            "[cron-module] Timer triggered. Do your periodic work here.",
        );

        if !(ngx_exiting == 1) && !(ngx_quit == 1) {
            let event: &mut Event = ev.into();
            event.add_timer(1000);
        }
    }
}

pub mod ngx_hyper {
    use hyper::rt::Executor;
    use ngx::ffi::ngx_event_t;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    // Custom waker that uses nginx events
    struct NgxWaker {
        event: *mut ngx_event_t,
    }

    impl NgxWaker {
        fn new(event: *mut ngx_event_t) -> Self {
            Self { event }
        }
    }

    unsafe impl Send for NgxWaker {}
    unsafe impl Sync for NgxWaker {}

    impl Wake for NgxWaker {
        fn wake(self: Arc<Self>) {
            unsafe {
                (*self.event).handler.unwrap()(self.event);
            }
        }

        fn wake_by_ref(self: &Arc<Self>) {
            unsafe {
                (*self.event).handler.unwrap()(self.event);
            }
        }
    }

    // Executor implementation that uses nginx's event loop
    #[derive(Clone)]
    pub struct NgxHyperExecutor {
        cycle: *mut ngx::ffi::ngx_cycle_t,
    }

    unsafe impl Send for NgxHyperExecutor {}
    unsafe impl Sync for NgxHyperExecutor {}

    impl NgxHyperExecutor {
        pub fn new(cycle: *mut ngx::ffi::ngx_cycle_t) -> Self {
            Self { cycle }
        }

        fn create_event(&self) -> *mut ngx_event_t {
            unsafe {
                let event =
                    ngx::ffi::ngx_pcalloc((*self.cycle).pool, std::mem::size_of::<ngx_event_t>() as libc::size_t)
                        as *mut ngx_event_t;

                (*event).log = (*self.cycle).log;
                event
            }
        }
    }

    impl<F> Executor<F> for NgxHyperExecutor
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        fn execute(&self, future: F) {
            let event = self.create_event();

            // Create waker using nginx event
            let waker = Arc::new(NgxWaker::new(event)).into();
            let mut context = Context::from_waker(&waker);

            // Pin the future and start polling
            let mut pinned = Box::pin(future);
            unsafe {
                (*event).data = Box::into_raw(Box::new(&pinned)) as *mut libc::c_void;
            }

            unsafe {
                // Set up the event handler
                (*event).handler = Some(event_done_handler);

                // Initial poll
                if let Poll::Pending = pinned.as_mut().poll(&mut context) {
                    // Future is pending, let it continue running
                    std::mem::forget(pinned);
                }
            }
        }
    }

    // Helper function to create executor from current nginx cycle
    pub fn create_executor(cycle: *mut ngx::ffi::ngx_cycle_t) -> NgxHyperExecutor {
        NgxHyperExecutor::new(cycle)
    }

    pub unsafe extern "C" fn event_done_handler(event: *mut ngx_event_t) {
        let waker = Arc::new(NgxWaker::new(event)).into();
        let mut context = Context::from_waker(&waker);

        let mut pinned = unsafe { Box::<Pin<&mut dyn Future<Output = ()>>>::from_raw((*event).data as *mut _) };
        if let Poll::Pending = pinned.as_mut().as_mut().poll(&mut context) {
            // Future is still pending, will be woken up later
            return;
        }
        // Future is complete, clean up the event
        // ngx::ffi::ngx_pfree((*(*event).pool).pool, event as *mut std::ffi::c_void);
    }

    pub fn handle_request(cycle: *mut ngx::ffi::ngx_cycle_t) {
        let executor = create_executor(cycle);
        let builder = hyper_util::client::legacy::Client::builder(executor);
        // let client = builder.build(hyper_util::client::legacy::Connector::new(executor));

        // hyper::rt::spawn(hyper::rt::Handle::new(executor), async {});
        // let client = hyper::Client::builder().executor(executor).build_http();
        // Use client...
    }
}
