// SPDX-License-Identifier: AGPL-3.0-or-later
// SDL owns the wl_display and wl_surface; all our proxies use a private queue.
#include <wayland-client.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "color-management-v1-client-protocol.h"

struct sg_hdr_surface {
    struct wl_display *display;
    struct wl_event_queue *queue;
    struct wl_registry *registry;
    struct wp_color_manager_v1 *manager;
    struct wp_color_management_surface_v1 *surface;
    struct wp_image_description_v1 *description;
    bool parametric, luminances, mastering, pq, bt2020, perceptual, ready, failed;
    char error[256];
};

static void intent(void *data, struct wp_color_manager_v1 *manager, uint32_t value) {
    (void)manager;
    if (value == WP_COLOR_MANAGER_V1_RENDER_INTENT_PERCEPTUAL)
        ((struct sg_hdr_surface *)data)->perceptual = true;
}
static void feature(void *data, struct wp_color_manager_v1 *manager, uint32_t value) {
    (void)manager;
    struct sg_hdr_surface *s = data;
    if (value == WP_COLOR_MANAGER_V1_FEATURE_PARAMETRIC) s->parametric = true;
    if (value == WP_COLOR_MANAGER_V1_FEATURE_SET_LUMINANCES) s->luminances = true;
    if (value == WP_COLOR_MANAGER_V1_FEATURE_SET_MASTERING_DISPLAY_PRIMARIES) s->mastering = true;
}
static void transfer(void *data, struct wp_color_manager_v1 *manager, uint32_t value) {
    (void)manager;
    if (value == WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_ST2084_PQ)
        ((struct sg_hdr_surface *)data)->pq = true;
}
static void primaries(void *data, struct wp_color_manager_v1 *manager, uint32_t value) {
    (void)manager;
    if (value == WP_COLOR_MANAGER_V1_PRIMARIES_BT2020)
        ((struct sg_hdr_surface *)data)->bt2020 = true;
}
static void done(void *data, struct wp_color_manager_v1 *manager) { (void)data; (void)manager; }
static const struct wp_color_manager_v1_listener manager_listener = {
    .supported_intent = intent, .supported_feature = feature,
    .supported_tf_named = transfer, .supported_primaries_named = primaries, .done = done,
};
static void global(void *data, struct wl_registry *registry, uint32_t name,
                   const char *interface, uint32_t version) {
    (void)version;
    struct sg_hdr_surface *s = data;
    if (!strcmp(interface, wp_color_manager_v1_interface.name)) {
        // Version 1 is enough; do not opt into events from newer revisions.
        s->manager = wl_registry_bind(registry, name, &wp_color_manager_v1_interface, 1);
        wp_color_manager_v1_add_listener(s->manager, &manager_listener, s);
    }
}
static void removed(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data; (void)registry; (void)name;
}
static const struct wl_registry_listener registry_listener = { global, removed };
static void failed(void *data, struct wp_image_description_v1 *description,
                   uint32_t cause, const char *message) {
    (void)description;
    struct sg_hdr_surface *s = data;
    s->failed = true;
    snprintf(s->error, sizeof(s->error), "HDR image description rejected (%u): %s", cause, message);
}
static void ready(void *data, struct wp_image_description_v1 *description, uint32_t identity) {
    (void)description; (void)identity;
    ((struct sg_hdr_surface *)data)->ready = true;
}
static const struct wp_image_description_v1_listener description_listener = {
    .failed = failed, .ready = ready,
};

void sg_hdr_destroy(struct sg_hdr_surface *s) {
    if (!s) return;
    // The wl_surface remains SDL-owned. It is destroyed immediately afterward.
    if (s->surface) wp_color_management_surface_v1_destroy(s->surface);
    if (s->description) wp_image_description_v1_destroy(s->description);
    if (s->manager) wp_color_manager_v1_destroy(s->manager);
    if (s->registry) wl_registry_destroy(s->registry);
    if (s->queue) wl_event_queue_destroy(s->queue);
    free(s);
}
static struct sg_hdr_surface *discover(struct wl_display *display) {
    struct sg_hdr_surface *s = calloc(1, sizeof(*s));
    if (!s) return NULL;
    s->display = display;
    s->queue = wl_display_create_queue(display);
    if (!s->queue) { sg_hdr_destroy(s); return NULL; }
    struct wl_display *wrapper = wl_proxy_create_wrapper(display);
    if (!wrapper) { sg_hdr_destroy(s); return NULL; }
    wl_proxy_set_queue((struct wl_proxy *)wrapper, s->queue);
    s->registry = wl_display_get_registry(wrapper);
    wl_proxy_wrapper_destroy(wrapper);
    wl_registry_add_listener(s->registry, &registry_listener, s);
    if (wl_display_roundtrip_queue(display, s->queue) < 0 ||
        wl_display_roundtrip_queue(display, s->queue) < 0 || !s->manager ||
        !s->parametric || !s->luminances || !s->mastering || !s->pq || !s->bt2020 || !s->perceptual) {
        sg_hdr_destroy(s);
        return NULL;
    }
    return s;
}

// Capability discovery only, not a promise that EGL/VAAPI can create the mode.
int sg_hdr_probe(void) {
    if (!getenv("WAYLAND_DISPLAY") && !getenv("WAYLAND_SOCKET")) return 0;
    const char *driver = getenv("SDL_VIDEODRIVER");
    if (driver && strcmp(driver, "wayland")) return 0;
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) return 0;
    struct sg_hdr_surface *s = discover(display);
    int supported = s != NULL;
    sg_hdr_destroy(s);
    wl_display_disconnect(display);
    return supported;
}

struct sg_hdr_metadata {
    uint32_t primaries[6], white[2], min_luminance, max_luminance, max_cll, max_fall, flags;
};

int sg_hdr_update(struct sg_hdr_surface *s, const struct sg_hdr_metadata *m,
                  char *error, size_t error_size) {
    struct wp_image_description_creator_params_v1 *params =
        wp_color_manager_v1_create_parametric_creator(s->manager);
    wp_image_description_creator_params_v1_set_primaries_named(params, WP_COLOR_MANAGER_V1_PRIMARIES_BT2020);
    wp_image_description_creator_params_v1_set_tf_named(params, WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_ST2084_PQ);
    if (s->luminances) wp_image_description_creator_params_v1_set_luminances(params, 0, 10000, 203);
    if (m && (m->flags & 1) && s->mastering) {
        wp_image_description_creator_params_v1_set_mastering_display_primaries(params,
            m->primaries[0] * 20, m->primaries[1] * 20, m->primaries[2] * 20, m->primaries[3] * 20,
            m->primaries[4] * 20, m->primaries[5] * 20, m->white[0] * 20, m->white[1] * 20);
        wp_image_description_creator_params_v1_set_mastering_luminance(params,
            m->min_luminance, (m->max_luminance + 9999) / 10000);
    }
    if (m && (m->flags & 2)) {
        // CTA zero means unspecified; do not invent a black content maximum.
        if (m->max_cll) wp_image_description_creator_params_v1_set_max_cll(params, m->max_cll);
        if (m->max_fall) wp_image_description_creator_params_v1_set_max_fall(params, m->max_fall);
    }
    struct wp_image_description_v1 *description = wp_image_description_creator_params_v1_create(params);
    s->ready = false; s->failed = false; s->error[0] = 0;
    wp_image_description_v1_add_listener(description, &description_listener, s);
    if (wl_display_roundtrip_queue(s->display, s->queue) < 0 || !s->ready || s->failed) {
        snprintf(error, error_size, "%s", s->error[0] ? s->error : "HDR metadata description was not acknowledged");
        wp_image_description_v1_destroy(description);
        return 0;
    }
    wp_color_management_surface_v1_set_image_description(s->surface, description, WP_COLOR_MANAGER_V1_RENDER_INTENT_PERCEPTUAL);
    if (s->description) wp_image_description_v1_destroy(s->description);
    s->description = description;
    return 1;
}

struct sg_hdr_surface *sg_hdr_attach(struct wl_display *display, struct wl_surface *surface,
                                     char *error, size_t error_size) {
    struct sg_hdr_surface *s = discover(display);
    if (!s) {
        snprintf(error, error_size, "Wayland compositor lacks parametric BT.2020/PQ color-management-v1 support");
        return NULL;
    }
    struct wp_image_description_creator_params_v1 *params =
        wp_color_manager_v1_create_parametric_creator(s->manager);
    wp_image_description_creator_params_v1_set_primaries_named(params, WP_COLOR_MANAGER_V1_PRIMARIES_BT2020);
    wp_image_description_creator_params_v1_set_tf_named(params, WP_COLOR_MANAGER_V1_TRANSFER_FUNCTION_ST2084_PQ);
    // Absolute PQ signal range, NOT invented mastering/display metadata.
    // Its black is 0 nits and reference white 203 nits (BT.2408).
    if (s->luminances) wp_image_description_creator_params_v1_set_luminances(params, 0, 10000, 203);
    s->description = wp_image_description_creator_params_v1_create(params);
    wp_image_description_v1_add_listener(s->description, &description_listener, s);
    if (wl_display_roundtrip_queue(s->display, s->queue) < 0 || !s->ready || s->failed) {
        snprintf(error, error_size, "%s", s->error[0] ? s->error : "HDR description was not acknowledged");
        sg_hdr_destroy(s);
        return NULL;
    }
    // SDL's GL backend only creates surface feedback, not a color surface.
    // This object controls the color description of subsequent EGL buffers.
    s->surface = wp_color_manager_v1_get_surface(s->manager, surface);
    wp_color_management_surface_v1_set_image_description(s->surface, s->description,
        WP_COLOR_MANAGER_V1_RENDER_INTENT_PERCEPTUAL);
    // Double-buffered: the first EGL swap commits this state with the pixels.
    if (wl_display_flush(s->display) < 0) {
        snprintf(error, error_size, "failed to flush HDR surface description");
        sg_hdr_destroy(s);
        return NULL;
    }
    return s;
}
