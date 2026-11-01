/* Typed access to the mimalloc options the edge node depends on.
 *
 * libmimalloc-sys 0.1.49 compiles mimalloc v3 by default, but its `extended`
 * Rust bindings still describe the v2 option enum (slots 12/21/26 are v2's
 * abandoned-page/segment-reclaim knobs). Guessing slots against v3 silently
 * addresses a different option: in v3 slot 26 is
 * `mi_option_disallow_arena_alloc`, so the pre-fix code turned arenas off
 * while believing it had enabled reclaim-on-free.
 *
 * Resolve the options through the header of the libmimalloc-sys build this
 * crate links (build.rs passes DEP_MIMALLOC_INCLUDE_DIR). No allocator source
 * is compiled here; these symbols resolve to the single mimalloc archive
 * already linked by libmimalloc-sys.
 */

#include <mimalloc.h>

#if MI_MALLOC_VERSION < 30300
#error "allocator_options.c requires the mimalloc v3 header (MI_MALLOC_VERSION >= 30300)"
#endif

/* Version burned into the header this translation unit was compiled with. */
int zelvik_allocator_header_version(void) {
    return MI_MALLOC_VERSION;
}

/* Version reported by the allocator archive actually linked into the binary. */
int zelvik_allocator_runtime_version(void) {
    return mi_version();
}

/* Named enum values; Rust must never duplicate the integer slots. */
int zelvik_allocator_page_reclaim_on_free_option(void) {
    return (int)mi_option_page_reclaim_on_free;
}

int zelvik_allocator_disallow_arena_alloc_option(void) {
    return (int)mi_option_disallow_arena_alloc;
}

long zelvik_allocator_option_get(int option) {
    return mi_option_get((mi_option_t)option);
}

void zelvik_allocator_option_set(int option, long value) {
    mi_option_set((mi_option_t)option, value);
}
