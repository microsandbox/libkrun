/* Test-only macOS interposer: fail exactly the second protection call after
 * the harness creates PR121_FAULT_FILE. Nothing is injected into production. */
#include <Hypervisor/Hypervisor.h>
#include <stdlib.h>
#include <unistd.h>
#include <stdatomic.h>

static _Atomic unsigned calls;
static hv_return_t fail_protect(hv_ipa_t addr, size_t size, hv_memory_flags_t flags) {
    const char *path = getenv("PR121_FAULT_FILE");
    if (path && access(path, F_OK) == 0 && atomic_fetch_add(&calls, 1) == 1) {
        unlink(path);
        return HV_ERROR;
    }
    return hv_vm_protect(addr, size, flags);
}

__attribute__((used)) static struct { const void *replacement; const void *original; }
interpose __attribute__((section("__DATA,__interpose"))) = {
    (const void *)fail_protect, (const void *)hv_vm_protect
};
