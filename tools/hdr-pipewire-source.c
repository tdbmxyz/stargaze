// SPDX-License-Identifier: AGPL-3.0-or-later
// Hardware-test producer: real RGB10 BT.2020/PQ SPA metadata and MemFd buffers.
// NOT desktop capture. Compile/run inside nix develop .#cuda:
// cc -O2 -Wall -Wextra tools/hdr-pipewire-source.c $(pkg-config --cflags --libs libpipewire-0.3) -o /tmp/hdr-pw-source
#define _POSIX_C_SOURCE 200809L
#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <spa/param/buffers.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

struct source { struct pw_main_loop *loop; struct pw_stream *stream; };
enum { WIDTH = 2048, HEIGHT = 256, STRIDE = WIDTH * 4 };
static void process(void *data) {
    struct source *s = data;
    struct pw_buffer *b = pw_stream_dequeue_buffer(s->stream);
    if (!b) return;
    struct spa_buffer *buffer = b->buffer;
    if (buffer->n_datas && buffer->datas[0].data && buffer->datas[0].maxsize >= STRIDE * HEIGHT) {
        struct spa_data *d = &buffer->datas[0];
        for (uint32_t y = 0; y < HEIGHT; y++) {
            uint32_t *row = (uint32_t *)((uint8_t *)d->data + y * STRIDE);
            for (uint32_t x = 0; x < WIDTH; x++) {
                uint32_t v = x / 2;
                row[x] = v | (v << 10) | (v << 20);
            }
        }
        d->chunk->offset = 0; d->chunk->size = STRIDE * HEIGHT; d->chunk->stride = STRIDE;
        b->size = 1;
    }
    pw_stream_queue_buffer(s->stream, b);
}
static void param_changed(void *data, uint32_t id, const struct spa_pod *param) {
    struct source *s = data;
    if (id != SPA_PARAM_Format || !param) return;
    uint8_t bytes[1024];
    struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(bytes, sizeof(bytes));
    const struct spa_pod *params[] = {
        spa_pod_builder_add_object(&builder, SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
            SPA_PARAM_BUFFERS_buffers, SPA_POD_CHOICE_RANGE_Int(4, 2, 8),
            SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
            SPA_PARAM_BUFFERS_size, SPA_POD_Int(STRIDE * HEIGHT),
            SPA_PARAM_BUFFERS_stride, SPA_POD_Int(STRIDE),
            SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(1 << SPA_DATA_MemFd))
    };
    pw_stream_update_params(s->stream, params, 1);
}
static void state_changed(void *data, enum pw_stream_state old, enum pw_stream_state state, const char *error) {
    (void)old;
    struct source *s = data;
    if (state == PW_STREAM_STATE_PAUSED) {
        printf("NODE_ID=%u\n", pw_stream_get_node_id(s->stream)); fflush(stdout);
    }
    if (state == PW_STREAM_STATE_ERROR) {
        fprintf(stderr, "PipeWire source: %s\n", error); pw_main_loop_quit(s->loop);
    }
}
static const struct pw_stream_events events = {
    PW_VERSION_STREAM_EVENTS, .state_changed = state_changed, .param_changed = param_changed, .process = process,
};
static void tick(void *data, uint64_t expirations) {
    (void)expirations;
    struct source *s = data;
    if (pw_stream_get_state(s->stream, NULL) == PW_STREAM_STATE_STREAMING)
        pw_stream_trigger_process(s->stream);
}
static void stop(void *data, int signal) { (void)signal; pw_main_loop_quit(((struct source *)data)->loop); }
int main(int argc, char **argv) {
    pw_init(&argc, &argv);
    struct source s = {0};
    s.loop = pw_main_loop_new(NULL);
    if (!s.loop) return 1;
    struct pw_loop *loop = pw_main_loop_get_loop(s.loop);
    s.stream = pw_stream_new_simple(loop, "stargaze-hdr-fixture",
        pw_properties_new(PW_KEY_MEDIA_TYPE,"Video", PW_KEY_MEDIA_CATEGORY,"Capture",
            PW_KEY_MEDIA_CLASS,"Video/Source", PW_KEY_NODE_SUPPORTS_REQUEST,"1", PW_KEY_NODE_NAME,"stargaze-hdr-fixture",
            PW_KEY_NODE_DESCRIPTION,"Stargaze explicit HDR test source", PW_KEY_NODE_LATENCY,"1/30", NULL), &events, &s);
    if (!s.stream) return 1;
    uint8_t bytes[1024];
    struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(bytes, sizeof(bytes));
    const struct spa_video_info_raw info = {
        .format = SPA_VIDEO_FORMAT_xBGR_210LE, .size = {WIDTH, HEIGHT}, .framerate = {30,1},
        .color_range = SPA_VIDEO_COLOR_RANGE_0_255, .color_matrix = SPA_VIDEO_COLOR_MATRIX_RGB,
        .transfer_function = SPA_VIDEO_TRANSFER_SMPTE2084, .color_primaries = SPA_VIDEO_COLOR_PRIMARIES_BT2020,
    };
    const struct spa_pod *params[] = {spa_format_video_raw_build(&builder, SPA_PARAM_EnumFormat, &info)};
    if (pw_stream_connect(s.stream, PW_DIRECTION_OUTPUT, PW_ID_ANY,
        PW_STREAM_FLAG_DRIVER | PW_STREAM_FLAG_MAP_BUFFERS, params, 1) < 0) return 1;
    struct spa_source *timer = pw_loop_add_timer(loop, tick, &s);
    struct timespec interval = {.tv_sec=0, .tv_nsec=33333333};
    pw_loop_update_timer(loop, timer, &interval, &interval, false);
    pw_loop_add_signal(loop, SIGINT, stop, &s); pw_loop_add_signal(loop, SIGTERM, stop, &s);
    pw_main_loop_run(s.loop);
    pw_stream_destroy(s.stream); pw_main_loop_destroy(s.loop); pw_deinit();
    return 0;
}
