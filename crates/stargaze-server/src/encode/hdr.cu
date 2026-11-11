// SPDX-License-Identifier: AGPL-3.0-or-later
// BT.2020 NCL on already-PQ RGB code values. No tone mapping / transfer change.
__device__ float3 read_hdr(const unsigned char *p, int layout) {
    if (layout == 3) {
        const unsigned short *v = (const unsigned short *)p;
        return make_float3(v[0] / 65535.0f, v[1] / 65535.0f, v[2] / 65535.0f);
    }
    unsigned int v = *(const unsigned int *)p;
    float lo = (v & 1023) / 1023.0f;
    float g = ((v >> 10) & 1023) / 1023.0f;
    float hi = ((v >> 20) & 1023) / 1023.0f;
    return layout == 1 ? make_float3(lo, g, hi) : make_float3(hi, g, lo);
}
__device__ unsigned short p010(float code) {
    return ((unsigned short)min(max((int)floorf(code + 0.5f), 0), 1023)) << 6;
}
extern "C" __global__ void rgba_to_p010(
    const unsigned char* __restrict__ src, unsigned long long src_pitch,
    unsigned char* __restrict__ y_plane, unsigned long long y_pitch,
    unsigned char* __restrict__ uv_plane, unsigned long long uv_pitch,
    int width, int height, int r_off, int g_off, int b_off, int layout)
{
    int x = (blockIdx.x * blockDim.x + threadIdx.x) * 2;
    int y = (blockIdx.y * blockDim.y + threadIdx.y) * 2;
    if (x >= width || y >= height) return;
    int bpp = layout == 3 ? 8 : 4;
    float3 sum = make_float3(0, 0, 0);
    for (int dy = 0; dy < 2; ++dy) {
        unsigned short *out = (unsigned short *)(y_plane + (y + dy) * y_pitch);
        for (int dx = 0; dx < 2; ++dx) {
            float3 v = read_hdr(src + (y + dy) * src_pitch + (x + dx) * bpp, layout);
            float l = 0.2627f * v.x + 0.6780f * v.y + 0.0593f * v.z;
            out[x + dx] = p010(64.0f + 876.0f * l);
            sum.x += v.x; sum.y += v.y; sum.z += v.z;
        }
    }
    float r = sum.x * 0.25f, g = sum.y * 0.25f, b = sum.z * 0.25f;
    float l = 0.2627f * r + 0.6780f * g + 0.0593f * b;
    unsigned short *uv = (unsigned short *)(uv_plane + (y / 2) * uv_pitch);
    uv[x] = p010(512.0f + 896.0f * (b - l) / 1.8814f);
    uv[x + 1] = p010(512.0f + 896.0f * (r - l) / 1.4746f);
}
