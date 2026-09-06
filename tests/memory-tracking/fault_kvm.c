/* Test-only failure after a successful, destructive dirty-log harvest. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <linux/kvm.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int ioctl(int fd, unsigned long request, ...) {
    static int (*real_ioctl)(int, unsigned long, ...);
    static unsigned count;
    va_list args;
    va_start(args, request);
    unsigned long argument = va_arg(args, unsigned long);
    va_end(args);
    if (!real_ioctl)
        real_ioctl = dlsym(RTLD_NEXT, "ioctl");
    const char *path = getenv("PR121_FAULT_FILE");
    const char *kind = getenv("PR121_FAULT_KIND");
    int target = request == KVM_GET_DIRTY_LOG;
    if (kind && strcmp(kind, "disable") == 0) {
        target = request == KVM_SET_USER_MEMORY_REGION &&
            !(((struct kvm_userspace_memory_region *)argument)->flags & KVM_MEM_LOG_DIRTY_PAGES);
    }
    if (target && path && access(path, F_OK) == 0) {
        if (++count == 2) {
            unlink(path);
            errno = EIO;
            return -1;
        }
    }
    return real_ioctl(fd, request, argument);
}
