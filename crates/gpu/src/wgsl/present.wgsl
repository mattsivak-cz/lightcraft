// Hand a render to the window without reading it back (issue #1228): copy the kept part of the
// per-pixel stage's packed RGBA8 output into rows padded for a buffer-to-texture copy, and count
// what the CPU path checks on the read-back image (`render::render`): pixels the kernel never
// wrote (alpha other than 255) and pixels that are not black.
// P: src_w, x0, y0, w, h (the kept part), stride (words per padded row). Bindings: src, dst, stats
// (stats[0] = unwritten, stats[1] = not black; cleared by the caller).

var<workgroup> wg_unwritten: atomic<u32>;
var<workgroup> wg_lit: atomic<u32>;

@compute @workgroup_size(16, 16)
fn pad_rows(@builtin(global_invocation_id) g: vec3<u32>, @builtin(local_invocation_index) li: u32) {
    let src_w = pu(0u);
    let x0 = pu(1u);
    let y0 = pu(2u);
    let w = pu(3u);
    let h = pu(4u);
    let stride = pu(5u);
    if (li == 0u) {
        atomicStore(&wg_unwritten, 0u);
        atomicStore(&wg_lit, 0u);
    }
    workgroupBarrier();
    if (g.x < w && g.y < h) {
        let v = src[(y0 + g.y) * src_w + x0 + g.x];
        dst[g.y * stride + g.x] = v;
        if ((v >> 24u) != 255u) {
            atomicAdd(&wg_unwritten, 1u);
        }
        if ((v & 0xffffffu) != 0u) {
            atomicAdd(&wg_lit, 1u);
        }
    }
    workgroupBarrier();
    if (li == 0u) {
        let u = atomicLoad(&wg_unwritten);
        let l = atomicLoad(&wg_lit);
        if (u > 0u) {
            atomicAdd(&stats[0], u);
        }
        if (l > 0u) {
            atomicAdd(&stats[1], l);
        }
    }
}
