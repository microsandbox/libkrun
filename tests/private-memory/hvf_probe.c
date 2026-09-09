// Native HVF qualification: does a guest store respect a private file mapping?
// Build with clang -framework Hypervisor and sign with hypervisor entitlements.
#include <Hypervisor/Hypervisor.h>
#include <assert.h>
#include <fcntl.h>
#include <mach/mach.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const size_t image_size = 256 * 1024 * 1024;
static const uint64_t initial_word = 0x1122334455667788ULL;

static uint64_t monotonic_ns(void) {
    struct timespec time;
    assert(clock_gettime(CLOCK_MONOTONIC, &time) == 0);
    return (uint64_t)time.tv_sec * 1000000000ULL + (uint64_t)time.tv_nsec;
}

static uint64_t resident_bytes(void) {
    mach_task_basic_info_data_t info;
    mach_msg_type_number_t count = MACH_TASK_BASIC_INFO_COUNT;
    assert(task_info(mach_task_self(), MACH_TASK_BASIC_INFO,
                     (task_info_t)&info, &count) == KERN_SUCCESS);
    return info.resident_size;
}

static void run_guest(int fd, uint64_t value, int ready_fd, int release_fd) {
    const size_t size = image_size;
    const size_t page = (size_t)sysconf(_SC_PAGESIZE);
    uint64_t mapping_started = monotonic_ns();
    void *guest = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    void *sibling = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    assert(guest != MAP_FAILED && sibling != MAP_FAILED);
    uint64_t before = resident_bytes();
    assert(hv_vm_create(NULL) == HV_SUCCESS);
    const uint64_t gpa = 0x80000000ULL;
    hv_return_t mapped = hv_vm_map(guest, gpa, size,
                                  HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC);
    printf("pid=%d map_result=0x%x mapping_us=%.2f resident_before=%llu resident_after=%llu\n",
           getpid(), mapped, (monotonic_ns() - mapping_started) / 1000.0,
           (unsigned long long)before, (unsigned long long)resident_bytes());
    fflush(stdout);
    assert(mapped == HV_SUCCESS);
    hv_vcpu_t cpu;
    hv_vcpu_exit_t *exit;
    assert(hv_vcpu_create(&cpu, &exit, NULL) == HV_SUCCESS);
    assert(hv_vcpu_set_reg(cpu, HV_REG_PC, gpa) == HV_SUCCESS);
    assert(hv_vcpu_set_reg(cpu, HV_REG_CPSR, 0x3c5) == HV_SUCCESS);
    assert(hv_vcpu_set_reg(cpu, HV_REG_X0, gpa + page) == HV_SUCCESS);
    assert(hv_vcpu_set_reg(cpu, HV_REG_X1, value) == HV_SUCCESS);
    // Both VMs must be registered before either writes. The backing has already been unlinked.
    assert(*(uint64_t *)((char *)guest + page) == initial_word);
    assert(write(ready_fd, "r", 1) == 1);
    char release;
    assert(read(release_fd, &release, 1) == 1);
    uint64_t run_started = monotonic_ns();
    assert(hv_vcpu_run(cpu) == HV_SUCCESS);
    uint64_t run_ns = monotonic_ns() - run_started;
    uint64_t disk = 0;
    assert(pread(fd, &disk, sizeof(disk), page) == sizeof(disk));
    uint64_t actual = *(uint64_t *)((char *)guest + page);
    uint64_t peer = *(uint64_t *)((char *)sibling + page);
    printf("pid=%d exit=%u syndrome=0x%llx run_us=%.2f guest=0x%llx sibling=0x%llx file=0x%llx resident=%llu\n",
           getpid(), exit->reason, (unsigned long long)exit->exception.syndrome,
           run_ns / 1000.0,
           (unsigned long long)actual, (unsigned long long)peer,
           (unsigned long long)disk, (unsigned long long)resident_bytes());
    assert(actual == value && peer == initial_word && disk == initial_word);
    // A host/device-style write must also privatize, including a page that was a sparse zero.
    *(uint64_t *)((char *)guest + page * 2) = value;
    assert(*(uint64_t *)((char *)sibling + page * 2) == 0);
    assert(pread(fd, &disk, sizeof(disk), page * 2) == sizeof(disk));
    assert(disk == 0);
    assert(hv_vcpu_destroy(cpu) == HV_SUCCESS);
    assert(hv_vm_unmap(gpa, size) == HV_SUCCESS);
    assert(hv_vm_destroy() == HV_SUCCESS);
    assert(munmap(guest, size) == 0);
    assert(munmap(sibling, size) == 0);
    close(fd);
    fflush(stdout);
}

int main(void) {
    const size_t page = (size_t)sysconf(_SC_PAGESIZE);
    char path[] = "/private/tmp/hvf-private-memory.XXXXXX";
    int writer = mkstemp(path);
    assert(writer >= 0);
    assert(ftruncate(writer, image_size) == 0);
    // str x1, [x0]; hvc #0. Data is separate from the executable page.
    uint32_t code[] = {0xf9000001, 0xd4000002};
    assert(pwrite(writer, code, sizeof(code), 0) == sizeof(code));
    assert(pwrite(writer, &initial_word, sizeof(initial_word), page) == sizeof(initial_word));
    assert(fsync(writer) == 0);
    assert(fchmod(writer, 0400) == 0);
    close(writer);
    int fd = open(path, O_RDONLY);
    assert(fd >= 0);
    assert(unlink(path) == 0);
    int ready[2], release[2];
    assert(pipe(ready) == 0 && pipe(release) == 0);
    pid_t children[2];
    for (unsigned index = 0; index < 2; index++) {
        children[index] = fork();
        assert(children[index] >= 0);
        if (children[index] == 0) {
            close(ready[0]);
            close(release[1]);
            run_guest(fd, 0xaabbccddeeff0011ULL + index, ready[1], release[0]);
            _exit(0);
        }
    }
    close(ready[1]);
    close(release[0]);
    for (unsigned index = 0; index < 2; index++) {
        char byte;
        assert(read(ready[0], &byte, 1) == 1);
    }
    assert(write(release[1], "gg", 2) == 2);
    close(ready[0]);
    close(release[1]);
    for (unsigned index = 0; index < 2; index++) {
        int status;
        assert(waitpid(children[index], &status, 0) == children[index]);
        assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    }
    close(fd);
    puts("PASS: independent HVF guests, guest/host writes, sparse zero and unlinked backing");
    return 0;
}
