// SPDX-License-Identifier: AGPL-3.0-or-later
// Single-threaded Wayland capture bridge. Each delivered DMA-BUF has independent
// storage; the transferred FD keeps it alive while the encoder consumes it.
#define _GNU_SOURCE
#include "wayland_capture.h"
#include "color-management-v1-client-protocol.h"
#include "ext-image-capture-source-v1-client-protocol.h"
#include "ext-image-copy-capture-v1-client-protocol.h"
#include "linux-dmabuf-v1-client-protocol.h"
#include "xx-image-capture-color-management-v1-client-protocol.h"
#include <drm_fourcc.h>
#include <errno.h>
#include <fcntl.h>
#include <gbm.h>
#include <limits.h>
#include <poll.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include <xf86drm.h>

struct sg_output {
    struct wl_output *proxy;
    uint32_t global;
    char name[256];
};
struct sg_capture {
    struct wl_display *display;
    struct wl_registry *registry;
    struct wl_shm *shm;
    struct wp_color_manager_v1 *color;
    struct xx_image_capture_color_manager_v1 *capture_color;
    struct ext_output_image_capture_source_manager_v1 *sources;
    struct ext_image_copy_capture_manager_v1 *copies;
    struct zwp_linux_dmabuf_v1 *linux_dma;
    struct sg_output outputs[32];
    unsigned output_count;
    uint32_t selected_global;
    struct ext_image_capture_source_v1 *source;
    struct xx_image_capture_source_colors_v1 *colors;
    struct wp_image_description_v1 *description;
    struct ext_image_copy_capture_session_v1 *session;
    struct ext_image_copy_capture_frame_v1 *frame;
    struct wl_buffer *buffer;
    struct wl_shm_pool *pool;
    struct gbm_device *device;
    struct gbm_bo *bo;
    int device_fd, buffer_fd;
    void *mapping;
    size_t mapping_size;
    dev_t device_id;
    uint64_t modifiers[256];
    size_t modifier_count;
    uint32_t width, height, stride, offset;
    bool dma, hdr, initialized, failed, shm10, parametric, luminances, pq, bt2020, srgb;
    int ready, info_done, constraints_done, frame_ready;
    uint32_t preferred_tf, preferred_primaries;
    uint64_t preferred_id;
    bool delivered;
    char error[256];
};

static void fail(struct sg_capture *s, const char *message) {
    if (!s->failed)
        snprintf(s->error, sizeof(s->error), "%s", message);
    s->failed = true;
}
static int dispatch(struct sg_capture *s, int timeout_ms, const int *condition) {
    if (s->failed)
        return -1;
    while (wl_display_prepare_read(s->display) != 0) {
        if (wl_display_dispatch_pending(s->display) < 0)
            goto error;
        if (s->failed)
            return -1;
        if (condition && *condition)
            return 1;
    }
    if (condition && *condition) {
        wl_display_cancel_read(s->display);
        return 1;
    }
    int flush = wl_display_flush(s->display);
    if (flush < 0 && errno != EAGAIN) {
        wl_display_cancel_read(s->display);
        goto error;
    }
    struct pollfd fd = {.fd = wl_display_get_fd(s->display), .events = POLLIN | (flush < 0 ? POLLOUT : 0)};
    int result = poll(&fd, 1, timeout_ms);
    if (result < 0 && errno == EINTR) {
        wl_display_cancel_read(s->display);
        return 0;
    }
    if (result < 0 || (fd.revents & (POLLERR | POLLHUP | POLLNVAL))) {
        wl_display_cancel_read(s->display);
        goto error;
    }
    if (fd.revents & POLLIN) {
        if (wl_display_read_events(s->display) < 0)
            goto error;
        if (wl_display_dispatch_pending(s->display) < 0)
            goto error;
    } else
        wl_display_cancel_read(s->display);
    return s->failed ? -1 : 0;
error:
    fail(s, "Wayland connection/protocol failure during capture");
    return -1;
}
static void sync_done(void *data, struct wl_callback *callback, uint32_t serial) {
    (void)callback;
    (void)serial;
    *(int *)data = 1;
}
static const struct wl_callback_listener sync_listener = {sync_done};
static bool roundtrip(struct sg_capture *s) {
    int done = 0;
    struct wl_callback *callback = wl_display_sync(s->display);
    if (!callback) {
        fail(s, "Cannot create Wayland sync callback");
        return false;
    }
    wl_callback_add_listener(callback, &sync_listener, &done);
    struct timespec started, now;
    clock_gettime(CLOCK_MONOTONIC, &started);
    while (!done && !s->failed) {
        if (dispatch(s, 50, &done) < 0)
            break;
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (now.tv_sec - started.tv_sec >= 8) {
            fail(s, "Wayland capture initialization timed out");
            break;
        }
    }
    wl_callback_destroy(callback);
    return done && !s->failed;
}

static void output_geometry(void *d, struct wl_output *o, int32_t x, int32_t y, int32_t w, int32_t h, int32_t sub,
                            const char *make, const char *model, int32_t transform) {
    (void)d;
    (void)o;
    (void)x;
    (void)y;
    (void)w;
    (void)h;
    (void)sub;
    (void)make;
    (void)model;
    (void)transform;
}
static void output_mode(void *d, struct wl_output *o, uint32_t f, int32_t w, int32_t h, int32_t hz) {
    (void)d;
    (void)o;
    (void)f;
    (void)w;
    (void)h;
    (void)hz;
}
static void output_done(void *d, struct wl_output *o) {
    (void)d;
    (void)o;
}
static void output_scale(void *d, struct wl_output *o, int32_t scale) {
    (void)d;
    (void)o;
    (void)scale;
}
static void output_name(void *d, struct wl_output *o, const char *name) {
    (void)o;
    snprintf(((struct sg_output *)d)->name, 256, "%s", name);
}
static void output_description(void *d, struct wl_output *o, const char *text) {
    (void)d;
    (void)o;
    (void)text;
}
static const struct wl_output_listener output_listener = {output_geometry, output_mode, output_done,
                                                          output_scale,    output_name, output_description};
static void shm_format(void *d, struct wl_shm *shm, uint32_t format) {
    (void)shm;
    if (format == WL_SHM_FORMAT_XBGR2101010)
        ((struct sg_capture *)d)->shm10 = true;
}
static const struct wl_shm_listener shm_listener = {shm_format};
static void color_intent(void *d, struct wp_color_manager_v1 *m, uint32_t v) {
    (void)d;
    (void)m;
    (void)v;
}
static void color_feature(void *d, struct wp_color_manager_v1 *m, uint32_t v) {
    (void)m;
    struct sg_capture *s = d;
    if (v == WP_COLOR_MANAGER_V1_FEATURE_PARAMETRIC)
        s->parametric = true;
    if (v == WP_COLOR_MANAGER_V1_FEATURE_SET_LUMINANCES)
        s->luminances = true;
}
static void color_tf(void *d, struct wp_color_manager_v1 *m, uint32_t v) {
    (void)m;
    struct sg_capture *s = d;
    if (v == WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_ST2084_PQ)
        s->pq = true;
    if (v == WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_COMPOUND_POWER_2_4)
        s->srgb = true;
}
static void color_primaries(void *d, struct wp_color_manager_v1 *m, uint32_t v) {
    (void)m;
    if (v == WP_COLOR_MANAGER_V1_PRIMARIES_BT2020)
        ((struct sg_capture *)d)->bt2020 = true;
}
static void color_done(void *d, struct wp_color_manager_v1 *m) {
    (void)d;
    (void)m;
}
static const struct wp_color_manager_v1_listener color_listener = {color_intent, color_feature, color_tf,
                                                                   color_primaries, color_done};
static void dma_format(void *d, struct zwp_linux_dmabuf_v1 *m, uint32_t f) {
    (void)d;
    (void)m;
    (void)f;
}
static void dma_modifier(void *d, struct zwp_linux_dmabuf_v1 *m, uint32_t f, uint32_t hi, uint32_t lo) {
    (void)d;
    (void)m;
    (void)f;
    (void)hi;
    (void)lo;
}
static const struct zwp_linux_dmabuf_v1_listener dma_listener = {dma_format, dma_modifier};
static void global(void *d, struct wl_registry *registry, uint32_t id, const char *name, uint32_t version) {
    struct sg_capture *s = d;
    if (!strcmp(name, "wl_shm")) {
        s->shm = wl_registry_bind(registry, id, &wl_shm_interface, 1);
        wl_shm_add_listener(s->shm, &shm_listener, s);
    } else if (!strcmp(name, "wp_color_manager_v1") && version >= 2) {
        s->color = wl_registry_bind(registry, id, &wp_color_manager_v1_interface, version < 3 ? version : 3);
        wp_color_manager_v1_add_listener(s->color, &color_listener, s);
    } else if (!strcmp(name, "xx_image_capture_color_manager_v1"))
        s->capture_color = wl_registry_bind(registry, id, &xx_image_capture_color_manager_v1_interface, 1);
    else if (!strcmp(name, "ext_output_image_capture_source_manager_v1"))
        s->sources = wl_registry_bind(registry, id, &ext_output_image_capture_source_manager_v1_interface, 1);
    else if (!strcmp(name, "ext_image_copy_capture_manager_v1"))
        s->copies = wl_registry_bind(registry, id, &ext_image_copy_capture_manager_v1_interface, 1);
    else if (!strcmp(name, "zwp_linux_dmabuf_v1") && version >= 3) {
        s->linux_dma = wl_registry_bind(registry, id, &zwp_linux_dmabuf_v1_interface, 3);
        zwp_linux_dmabuf_v1_add_listener(s->linux_dma, &dma_listener, s);
    } else if (!strcmp(name, "wl_output") && version >= 4 && s->output_count < 32) {
        struct sg_output *o = &s->outputs[s->output_count++];
        o->global = id;
        o->proxy = wl_registry_bind(registry, id, &wl_output_interface, 4);
        wl_output_add_listener(o->proxy, &output_listener, o);
    }
}
static void removed(void *d, struct wl_registry *r, uint32_t id) {
    (void)r;
    struct sg_capture *s = d;
    if (s->selected_global == id)
        fail(s, "Wayland capture output was removed");
}
static const struct wl_registry_listener registry_listener = {global, removed};
static void preferred_changed(void *d, struct xx_image_capture_source_colors_v1 *c, uint32_t hi, uint32_t lo) {
    (void)c;
    struct sg_capture *s = d;
    uint64_t id = ((uint64_t)hi << 32) | lo;
    if (s->initialized && s->preferred_id != id)
        fail(s, "Capture preferred color changed; reconnect required");
    s->preferred_id = id;
}
static const struct xx_image_capture_source_colors_v1_listener colors_listener = {preferred_changed};
static void description_failed(void *d, struct wp_image_description_v1 *p, uint32_t cause, const char *message) {
    (void)p;
    (void)cause;
    struct sg_capture *s = d;
    fail(s, message);
}
static void description_ready(void *d, struct wp_image_description_v1 *p, uint32_t id) {
    (void)p;
    (void)id;
    ((struct sg_capture *)d)->ready = 1;
}
static void description_ready2(void *d, struct wp_image_description_v1 *p, uint32_t hi, uint32_t lo) {
    (void)hi;
    description_ready(d, p, lo);
}
static const struct wp_image_description_v1_listener description_listener = {description_failed, description_ready,
                                                                             description_ready2};
static void info_done(void *d, struct wp_image_description_info_v1 *i) {
    (void)i;
    ((struct sg_capture *)d)->info_done = 1;
}
static void info_icc(void *d, struct wp_image_description_info_v1 *i, int32_t fd, uint32_t size) {
    (void)d;
    (void)i;
    (void)size;
    close(fd);
}
static void info_primaries(void *d, struct wp_image_description_info_v1 *i, int32_t rx, int32_t ry, int32_t gx,
                           int32_t gy, int32_t bx, int32_t by, int32_t wx, int32_t wy) {
    (void)d;
    (void)i;
    (void)rx;
    (void)ry;
    (void)gx;
    (void)gy;
    (void)bx;
    (void)by;
    (void)wx;
    (void)wy;
}
static void info_named(void *d, struct wp_image_description_info_v1 *i, uint32_t v) {
    (void)i;
    ((struct sg_capture *)d)->preferred_primaries = v;
}
static void info_tf(void *d, struct wp_image_description_info_v1 *i, uint32_t v) {
    (void)i;
    ((struct sg_capture *)d)->preferred_tf = v;
}
static void info_power(void *d, struct wp_image_description_info_v1 *i, uint32_t v) {
    (void)d;
    (void)i;
    (void)v;
}
static void info_luminances(void *d, struct wp_image_description_info_v1 *i, uint32_t min, uint32_t max, uint32_t ref) {
    (void)d;
    (void)i;
    (void)min;
    (void)max;
    (void)ref;
}
static void info_target_luminance(void *d, struct wp_image_description_info_v1 *i, uint32_t min, uint32_t max) {
    (void)d;
    (void)i;
    (void)min;
    (void)max;
}
static const struct wp_image_description_info_v1_listener info_listener = {
    .done = info_done,
    .icc_file = info_icc,
    .primaries = info_primaries,
    .primaries_named = info_named,
    .tf_power = info_power,
    .tf_named = info_tf,
    .luminances = info_luminances,
    .target_primaries = info_primaries,
    .target_luminance = info_target_luminance,
    .target_max_cll = info_power,
    .target_max_fall = info_power,
};
static void size_event(void *d, struct ext_image_copy_capture_session_v1 *session, uint32_t w, uint32_t h) {
    (void)session;
    struct sg_capture *s = d;
    if (s->initialized && (w != s->width || h != s->height)) {
        fail(s, "Capture geometry changed; reconnect required");
        return;
    }
    s->width = w;
    s->height = h;
}
static void shm_event(void *d, struct ext_image_copy_capture_session_v1 *session, uint32_t format) {
    (void)session;
    if (format == WL_SHM_FORMAT_XBGR2101010)
        ((struct sg_capture *)d)->shm10 = true;
}
static void device_event(void *d, struct ext_image_copy_capture_session_v1 *session, struct wl_array *device) {
    (void)session;
    struct sg_capture *s = d;
    if (device->size != sizeof(s->device_id))
        fail(s, "Invalid capture device");
    else
        memcpy(&s->device_id, device->data, sizeof(s->device_id));
}
static void modifiers_event(void *d, struct ext_image_copy_capture_session_v1 *session, uint32_t format,
                            struct wl_array *mods) {
    (void)session;
    struct sg_capture *s = d;
    if (format != DRM_FORMAT_XBGR2101010)
        return;
    if (mods->size % 8 || mods->size > sizeof(s->modifiers)) {
        fail(s, "Invalid/excessive capture modifiers");
        return;
    }
    s->modifier_count = mods->size / 8;
    if (mods->size)
        memcpy(s->modifiers, mods->data, mods->size);
}
static void constraints_done(void *d, struct ext_image_copy_capture_session_v1 *session) {
    (void)session;
    ((struct sg_capture *)d)->constraints_done = 1;
}
static void stopped(void *d, struct ext_image_copy_capture_session_v1 *session) {
    (void)session;
    fail(d, "Wayland capture session stopped");
}
static const struct ext_image_copy_capture_session_v1_listener session_listener = {
    size_event, shm_event, device_event, modifiers_event, constraints_done, stopped};
static void frame_transform(void *d, struct ext_image_copy_capture_frame_v1 *f, uint32_t transform) {
    (void)f;
    if (transform != WL_OUTPUT_TRANSFORM_NORMAL)
        fail(d, "Unsupported capture transform");
}
static void frame_damage(void *d, struct ext_image_copy_capture_frame_v1 *f, int32_t x, int32_t y, int32_t w,
                         int32_t h) {
    (void)d;
    (void)f;
    (void)x;
    (void)y;
    (void)w;
    (void)h;
}
static void frame_time(void *d, struct ext_image_copy_capture_frame_v1 *f, uint32_t hi, uint32_t lo, uint32_t ns) {
    (void)d;
    (void)f;
    (void)hi;
    (void)lo;
    (void)ns;
}
static void frame_ready(void *d, struct ext_image_copy_capture_frame_v1 *f) {
    (void)f;
    ((struct sg_capture *)d)->frame_ready = 1;
}
static void frame_failed(void *d, struct ext_image_copy_capture_frame_v1 *f, uint32_t reason) {
    (void)f;
    char error[128];
    snprintf(error, sizeof(error), "Wayland capture frame failed (%u)", reason);
    fail(d, error);
}
static const struct ext_image_copy_capture_frame_v1_listener frame_listener = {frame_transform, frame_damage,
                                                                               frame_time, frame_ready, frame_failed};
static void buffer_release(void *d, struct wl_buffer *b) {
    (void)d;
    (void)b;
}
static const struct wl_buffer_listener buffer_listener = {buffer_release};

static void release_frame(struct sg_capture *s) {
    if (s->frame)
        ext_image_copy_capture_frame_v1_destroy(s->frame);
    if (s->buffer)
        wl_buffer_destroy(s->buffer);
    if (s->pool)
        wl_shm_pool_destroy(s->pool);
    if (s->mapping)
        munmap(s->mapping, s->mapping_size);
    if (s->bo)
        gbm_bo_destroy(s->bo);
    if (s->buffer_fd >= 0)
        close(s->buffer_fd);
    s->frame = NULL;
    s->buffer = NULL;
    s->pool = NULL;
    s->mapping = NULL;
    s->bo = NULL;
    s->buffer_fd = -1;
    s->frame_ready = 0;
    s->delivered = false;
}
void sg_capture_close(struct sg_capture *s) {
    if (!s)
        return;
    release_frame(s);
    if (s->session)
        ext_image_copy_capture_session_v1_destroy(s->session);
    if (s->description)
        wp_image_description_v1_destroy(s->description);
    if (s->colors)
        xx_image_capture_source_colors_v1_destroy(s->colors);
    if (s->source)
        ext_image_capture_source_v1_destroy(s->source);
    for (unsigned i = 0; i < s->output_count; i++)
        wl_output_release(s->outputs[i].proxy);
    if (s->linux_dma)
        zwp_linux_dmabuf_v1_destroy(s->linux_dma);
    if (s->copies)
        ext_image_copy_capture_manager_v1_destroy(s->copies);
    if (s->sources)
        ext_output_image_capture_source_manager_v1_destroy(s->sources);
    if (s->capture_color)
        xx_image_capture_color_manager_v1_destroy(s->capture_color);
    if (s->color)
        wp_color_manager_v1_destroy(s->color);
    if (s->shm)
        wl_shm_destroy(s->shm);
    if (s->registry)
        wl_registry_destroy(s->registry);
    if (s->display)
        wl_display_disconnect(s->display);
    if (s->device)
        gbm_device_destroy(s->device);
    if (s->device_fd >= 0)
        close(s->device_fd);
    free(s);
}
const char *sg_capture_error(struct sg_capture *s) { return s->error; }
static bool has_modifier(struct sg_capture *s, uint64_t modifier) {
    for (size_t i = 0; i < s->modifier_count; i++)
        if (s->modifiers[i] == modifier)
            return true;
    return false;
}
static bool setup_device(struct sg_capture *s) {
    drmDevice *device = NULL;
    if (drmGetDeviceFromDevId(s->device_id, 0, &device)) {
        fail(s, "Cannot resolve capture render device");
        return false;
    }
    if (device->available_nodes & (1 << DRM_NODE_RENDER))
        s->device_fd = open(device->nodes[DRM_NODE_RENDER], O_RDWR | O_CLOEXEC);
    drmFreeDevice(&device);
    if (s->device_fd < 0 || !(s->device = gbm_create_device(s->device_fd))) {
        fail(s, "Cannot open capture GBM render device");
        return false;
    }
    return true;
}
struct sg_capture *sg_capture_open(const char *output, int cursor, int dma, int allow_hdr, char *error,
                                   size_t error_size) {
    struct sg_capture *s = calloc(1, sizeof(*s));
    if (!s) {
        snprintf(error, error_size, "Capture allocation failed");
        return NULL;
    }
    s->device_fd = -1;
    s->buffer_fd = -1;
    s->dma = dma != 0;
    s->display = wl_display_connect(NULL);
    if (!s->display) {
        fail(s, "Cannot connect to the selected Wayland display");
        goto failed;
    }
    s->registry = wl_display_get_registry(s->display);
    wl_registry_add_listener(s->registry, &registry_listener, s);
    if (!roundtrip(s) || !roundtrip(s))
        goto failed;
    if (!s->color || !s->capture_color || !s->sources || !s->copies || !s->shm || !s->parametric || !s->luminances ||
        !s->srgb) {
        fail(s, "Compositor lacks opt-in color-managed image-copy capture; use the portal for SDR");
        goto failed;
    }
    struct wl_output *selected = NULL;
    for (unsigned i = 0; i < s->output_count; i++)
        if (!strcmp(s->outputs[i].name, output)) {
            selected = s->outputs[i].proxy;
            s->selected_global = s->outputs[i].global;
            break;
        }
    if (!selected) {
        fail(s, "Selected Wayland output was not found");
        goto failed;
    }
    s->source = ext_output_image_capture_source_manager_v1_create_source(s->sources, selected);
    s->colors = xx_image_capture_color_manager_v1_get_capture_source_colors(s->capture_color, s->source);
    xx_image_capture_source_colors_v1_add_listener(s->colors, &colors_listener, s);
    struct wp_image_description_reference_v1 *reference = xx_image_capture_source_colors_v1_get_preferred(s->colors);
    struct wp_image_description_v1 *preferred = wp_color_manager_v1_get_image_description(s->color, reference);
    wp_image_description_reference_v1_destroy(reference);
    wp_image_description_v1_add_listener(preferred, &description_listener, s);
    bool ready = roundtrip(s) && s->ready;
    if (!ready) {
        wp_image_description_v1_destroy(preferred);
        fail(s, "Preferred capture description was not ready");
        goto failed;
    }
    struct wp_image_description_info_v1 *info = wp_image_description_v1_get_information(preferred);
    wp_image_description_info_v1_add_listener(info, &info_listener, s);
    ready = roundtrip(s) && s->info_done;
    wp_image_description_info_v1_destroy(info);
    wp_image_description_v1_destroy(preferred);
    if (!ready) {
        fail(s, "Preferred capture information was unavailable");
        goto failed;
    }
    s->hdr = allow_hdr && s->preferred_tf == WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_ST2084_PQ &&
             s->preferred_primaries == WP_COLOR_MANAGER_V1_PRIMARIES_BT2020;
    if (s->hdr && (!s->pq || !s->bt2020)) {
        fail(s, "Compositor cannot create the requested PQ/BT.2020 contract");
        goto failed;
    }
    struct wp_image_description_creator_params_v1 *params = wp_color_manager_v1_create_parametric_creator(s->color);
    wp_image_description_creator_params_v1_set_tf_named(
        params, s->hdr ? WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_ST2084_PQ
                       : WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_COMPOUND_POWER_2_4);
    wp_image_description_creator_params_v1_set_primaries_named(params, s->hdr ? WP_COLOR_MANAGER_V1_PRIMARIES_BT2020
                                                                              : WP_COLOR_MANAGER_V1_PRIMARIES_SRGB);
    wp_image_description_creator_params_v1_set_luminances(params, s->hdr ? 0 : 2000, s->hdr ? 10000 : 80,
                                                          s->hdr ? 203 : 80);
    s->description = wp_image_description_creator_params_v1_create(params);
    s->ready = 0;
    wp_image_description_v1_add_listener(s->description, &description_listener, s);
    if (!roundtrip(s) || !s->ready) {
        fail(s, "Requested capture contract was not acknowledged");
        goto failed;
    }
    s->shm10 = false;
    s->session = ext_image_copy_capture_manager_v1_create_session(
        s->copies, s->source, cursor ? EXT_IMAGE_COPY_CAPTURE_MANAGER_V1_OPTIONS_PAINT_CURSORS : 0);
    ext_image_copy_capture_session_v1_add_listener(s->session, &session_listener, s);
    if (!roundtrip(s) || !s->constraints_done) {
        fail(s, "Capture constraints were unavailable");
        goto failed;
    }
    if (!s->width || !s->height || s->width > 8192 || s->height > 8192 || (!s->dma && !s->shm10)) {
        fail(s, "Unsupported capture geometry/RGB10 SHM format");
        goto failed;
    }
    if (s->dma && (!s->linux_dma || !s->modifier_count || !setup_device(s))) {
        fail(s, "RGB10 DMA-BUF capture unavailable; select explicit Wayland SHM capture");
        goto failed;
    }
    s->stride = s->width * 4;
    s->mapping_size = (size_t)s->stride * s->height;
    s->initialized = true;
    return s;
failed:
    snprintf(error, error_size, "%s", s->error[0] ? s->error : "Wayland capture initialization failed");
    sg_capture_close(s);
    return NULL;
}
int sg_capture_format(struct sg_capture *s, uint32_t *width, uint32_t *height, uint32_t *hdr) {
    if (s->failed)
        return 0;
    *width = s->width;
    *height = s->height;
    *hdr = s->hdr;
    return 1;
}
static bool create_buffer(struct sg_capture *s) {
    if (!s->dma) {
        s->buffer_fd = memfd_create("stargaze-wayland-rgb10", MFD_CLOEXEC);
        if (s->buffer_fd < 0 || ftruncate(s->buffer_fd, (off_t)s->mapping_size))
            goto failed;
        s->mapping = mmap(NULL, s->mapping_size, PROT_READ | PROT_WRITE, MAP_SHARED, s->buffer_fd, 0);
        if (s->mapping == MAP_FAILED) {
            s->mapping = NULL;
            goto failed;
        }
        s->pool = wl_shm_create_pool(s->shm, s->buffer_fd, (int32_t)s->mapping_size);
        s->buffer = wl_shm_pool_create_buffer(s->pool, 0, s->width, s->height, s->stride, WL_SHM_FORMAT_XBGR2101010);
    } else {
        for (size_t i = 0; i < s->modifier_count && !s->bo; i++) {
            if (s->modifiers[i] == DRM_FORMAT_MOD_INVALID)
                continue;
            s->bo = gbm_bo_create_with_modifiers2(s->device, s->width, s->height, DRM_FORMAT_XBGR2101010,
                                                  &s->modifiers[i], 1, GBM_BO_USE_RENDERING);
        }
        if (!s->bo && has_modifier(s, DRM_FORMAT_MOD_INVALID))
            s->bo = gbm_bo_create(s->device, s->width, s->height, DRM_FORMAT_XBGR2101010, GBM_BO_USE_RENDERING);
        if (!s->bo || gbm_bo_get_plane_count(s->bo) != 1)
            goto failed;
        uint64_t modifier = gbm_bo_get_modifier(s->bo);
        if (!has_modifier(s, modifier) && !has_modifier(s, DRM_FORMAT_MOD_INVALID))
            goto failed;
        s->buffer_fd = gbm_bo_get_fd_for_plane(s->bo, 0);
        if (s->buffer_fd < 0)
            goto failed;
        s->stride = gbm_bo_get_stride_for_plane(s->bo, 0);
        s->offset = gbm_bo_get_offset(s->bo, 0);
        struct zwp_linux_buffer_params_v1 *params = zwp_linux_dmabuf_v1_create_params(s->linux_dma);
        zwp_linux_buffer_params_v1_add(params, s->buffer_fd, 0, s->offset, s->stride, modifier >> 32,
                                       (uint32_t)modifier);
        s->buffer = zwp_linux_buffer_params_v1_create_immed(params, s->width, s->height, DRM_FORMAT_XBGR2101010, 0);
        zwp_linux_buffer_params_v1_destroy(params);
    }
    if (!s->buffer)
        goto failed;
    wl_buffer_add_listener(s->buffer, &buffer_listener, s);
    return true;
failed:
    fail(s, "Failed to allocate independent RGB10 capture storage (no precision fallback)");
    return false;
}
int sg_capture_next(struct sg_capture *s, int timeout_ms, struct sg_capture_frame *result) {
    if (s->failed)
        return -1;
    if (s->delivered)
        release_frame(s);
    if (!s->frame) {
        if (!create_buffer(s))
            return -1;
        s->frame = ext_image_copy_capture_session_v1_create_frame(s->session);
        ext_image_copy_capture_frame_v1_add_listener(s->frame, &frame_listener, s);
        xx_image_capture_color_manager_v1_set_frame_image_description(s->capture_color, s->frame, s->description);
        ext_image_copy_capture_frame_v1_attach_buffer(s->frame, s->buffer);
        ext_image_copy_capture_frame_v1_damage_buffer(s->frame, 0, 0, s->width, s->height);
        ext_image_copy_capture_frame_v1_capture(s->frame);
    }
    if (!s->frame_ready && dispatch(s, timeout_ms, &s->frame_ready) < 0)
        return -1;
    if (s->failed)
        return -1;
    if (!s->frame_ready)
        return 0;
    *result = (struct sg_capture_frame){.fd = -1,
                                        .data = s->mapping,
                                        .width = s->width,
                                        .height = s->height,
                                        .stride = s->stride,
                                        .offset = s->offset,
                                        .hdr = s->hdr,
                                        .fourcc = DRM_FORMAT_XBGR2101010,
                                        .modifier = DRM_FORMAT_MOD_INVALID};
    if (s->dma) {
        result->fd = s->buffer_fd;
        s->buffer_fd = -1;
        result->modifier = gbm_bo_get_modifier(s->bo);
    }
    s->delivered = true;
    return 1;
}
