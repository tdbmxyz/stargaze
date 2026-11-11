// SPDX-License-Identifier: AGPL-3.0-or-later
#pragma once
#include <stddef.h>
#include <stdint.h>

struct sg_capture;
struct sg_capture_frame {
    int32_t fd;
    const uint8_t *data;
    uint32_t width, height, stride, offset;
    uint64_t modifier;
    uint32_t hdr, fourcc;
};
struct sg_capture *sg_capture_open(const char *output, int cursor, int dma, int allow_hdr, char *error,
                                   size_t error_size);
void sg_capture_close(struct sg_capture *capture);
int sg_capture_format(struct sg_capture *capture, uint32_t *width, uint32_t *height, uint32_t *hdr);
// 1 = ready, 0 = bounded wait, -1 = terminal error. On ready the FD is transferred
// to the caller, or CPU data is borrowed until the next next/close call.
int sg_capture_next(struct sg_capture *capture, int timeout_ms, struct sg_capture_frame *frame);
const char *sg_capture_error(struct sg_capture *capture);
