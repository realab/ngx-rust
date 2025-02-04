#include <ngx_http.h>
#include <ngx_conf_file.h>
#include <ngx_config.h>
#include <ngx_core.h>
#include <ngx_event_timer.h>

void ngx_event_add_timer_wrapper(ngx_event_t *ev, ngx_msec_t timer)
{
    ngx_event_add_timer(ev, timer);
}

void ngx_event_del_timer_wrapper(ngx_event_t *ev)
{
    ngx_event_del_timer(ev);
}

const char *NGX_RS_MODULE_SIGNATURE = NGX_MODULE_SIGNATURE;

// `--prefix=` results in not emitting the declaration
#ifndef NGX_PREFIX
#define NGX_PREFIX ""
#endif

#ifndef NGX_CONF_PREFIX
#define NGX_CONF_PREFIX NGX_PREFIX
#endif
