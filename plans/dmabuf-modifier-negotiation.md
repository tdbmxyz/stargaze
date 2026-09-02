# DMA-BUF Modifier Negotiation

## Issues to Address

- Zeus negotiates PipeWire modifier `0`, so capture falls back to 19.8 MB BGRA MemFd frames at 3440×1440.
- The fallback performs a GPU-to-CPU readback followed by CPU copies and a CPU-to-GPU upload for every frame.
- Stargaze advertises only `DRM_FORMAT_MOD_INVALID` instead of the concrete modifiers supported by the NVIDIA EGL implementation.

## Important Notes

- Keep the MemFd path as a safe fallback when EGL queries fail or no explicit non-linear modifier is shared with the compositor.
- Modifier `0` and `DRM_FORMAT_MOD_INVALID` must not activate the NVIDIA DMA-BUF path; both have produced corrupted imports on the target driver.
- Preserve the exact DRM fourcc negotiated for each SPA format so EGL imports alpha and opaque format variants correctly.
- Runtime validation on Zeus is required. Compilation and unit tests cannot prove that PipeWire selected DMA-BUF or that imported frames are visually correct.

## Implementation Strategy

- Query `eglQueryDmaBufFormatsEXT` and `eglQueryDmaBufModifiersEXT` on the same NVIDIA GBM-backed EGL device used by the encoder.
- Associate supported DRM fourcc values and concrete modifiers with PipeWire SPA video formats.
- Build one DMA-BUF format pod per supported format with its explicit modifier list and retain the SHM fallback pod.
- Carry the negotiated DRM fourcc with DMA-BUF frame metadata and use it for EGL image import.
- Add diagnostics that show queried modifiers and the selected capture path.

## Tests

- Unit-test SPA format-to-DRM-fourcc mappings and explicit modifier pod construction.
- Run workspace formatting, checking, clippy, and tests.
- Build the CUDA server package.
- On Zeus, verify a nonzero modifier, `DataType::DmaBuf`, the fully-GPU encoder path, correct image output, and reduced PCIe traffic/GPU overhead.
