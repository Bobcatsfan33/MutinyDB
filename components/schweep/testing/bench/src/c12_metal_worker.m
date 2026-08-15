#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#import <mach/mach_time.h>

typedef struct {
    int64_t key;
    int64_t value;
} C12Pair;

static uint64_t elapsed_nanos(uint64_t start, uint64_t end) {
    mach_timebase_info_data_t info;
    mach_timebase_info(&info);
    return (end - start) * info.numer / info.denom;
}

static void fail(NSString *message) {
    fprintf(stderr, "%s\n", message.UTF8String);
    exit(1);
}

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc != 2) {
            fail(@"usage: c12_metal_worker INPUT");
        }

        NSData *input = [NSData dataWithContentsOfFile:[NSString stringWithUTF8String:argv[1]]];
        if (input == nil || input.length % sizeof(C12Pair) != 0) {
            fail(@"C12 input is missing or is not a whole number of Int64 pairs");
        }

        uint64_t setup_start = mach_absolute_time();
        id<MTLDevice> device = MTLCreateSystemDefaultDevice();
        if (device == nil) {
            fail(@"no Metal device is available");
        }
        id<MTLCommandQueue> queue = [device newCommandQueue];
        if (queue == nil) {
            fail(@"Metal command queue creation failed");
        }

        NSString *source = @R"METAL(
#include <metal_stdlib>
using namespace metal;

struct Pair {
    long key;
    long value;
};

kernel void filter_sum(
    device const Pair *pairs [[buffer(0)]],
    device long *partials [[buffer(1)]],
    constant ulong &count [[buffer(2)]],
    constant long &threshold [[buffer(3)]],
    uint lane [[thread_index_in_threadgroup]],
    uint global [[thread_position_in_grid]],
    uint group [[threadgroup_position_in_grid]]) {
    threadgroup long sums[256];
    sums[lane] = global < count && pairs[global].key > threshold ? pairs[global].value : 0;
    threadgroup_barrier(mem_flags::mem_threadgroup);
    for (uint stride = 128; stride > 0; stride >>= 1) {
        if (lane < stride) {
            sums[lane] += sums[lane + stride];
        }
        threadgroup_barrier(mem_flags::mem_threadgroup);
    }
    if (lane == 0) {
        partials[group] = sums[0];
    }
}
)METAL";
        NSError *error = nil;
        id<MTLLibrary> library = [device newLibraryWithSource:source options:nil error:&error];
        if (library == nil) {
            fail([NSString stringWithFormat:@"Metal runtime compilation failed: %@", error]);
        }
        id<MTLFunction> function = [library newFunctionWithName:@"filter_sum"];
        id<MTLComputePipelineState> pipeline =
            [device newComputePipelineStateWithFunction:function error:&error];
        if (pipeline == nil) {
            fail([NSString stringWithFormat:@"Metal pipeline creation failed: %@", error]);
        }
        const NSUInteger threads = 256;
        if (pipeline.maxTotalThreadsPerThreadgroup < threads) {
            fail(@"Metal device cannot dispatch the frozen 256-thread C12 kernel");
        }
        uint64_t setup_nanos = elapsed_nanos(setup_start, mach_absolute_time());
        printf("READY\t%llu\t%s\n", setup_nanos, device.name.UTF8String);
        fflush(stdout);

        char line[128];
        while (fgets(line, sizeof(line), stdin) != NULL) {
            if (strncmp(line, "STOP", 4) == 0) {
                break;
            }
            unsigned long long raw_count = 0;
            if (sscanf(line, "RUN %llu", &raw_count) != 1) {
                fail(@"expected RUN <rows>");
            }
            NSUInteger count = (NSUInteger)raw_count;
            if (count > input.length / sizeof(C12Pair)) {
                fail(@"requested C12 size exceeds the input");
            }
            NSUInteger groups = (count + threads - 1) / threads;
            id<MTLBuffer> values = [device newBufferWithBytes:input.bytes
                                                       length:count * sizeof(C12Pair)
                                                      options:MTLResourceStorageModeShared];
            id<MTLBuffer> partials = [device newBufferWithLength:groups * sizeof(int64_t)
                                                         options:MTLResourceStorageModeShared];
            id<MTLCommandBuffer> command = [queue commandBuffer];
            id<MTLComputeCommandEncoder> encoder = [command computeCommandEncoder];
            if (values == nil || partials == nil || command == nil || encoder == nil) {
                fail(@"Metal per-round resource allocation failed");
            }
            uint64_t count64 = count;
            int64_t threshold = 0;
            [encoder setComputePipelineState:pipeline];
            [encoder setBuffer:values offset:0 atIndex:0];
            [encoder setBuffer:partials offset:0 atIndex:1];
            [encoder setBytes:&count64 length:sizeof(count64) atIndex:2];
            [encoder setBytes:&threshold length:sizeof(threshold) atIndex:3];
            [encoder dispatchThreads:MTLSizeMake(groups * threads, 1, 1)
                  threadsPerThreadgroup:MTLSizeMake(threads, 1, 1)];
            [encoder endEncoding];
            [command commit];
            [command waitUntilCompleted];
            if (command.status != MTLCommandBufferStatusCompleted) {
                fail([NSString stringWithFormat:@"Metal command failed: %@", command.error]);
            }

            const int64_t *parts = static_cast<const int64_t *>(partials.contents);
            int64_t total = 0;
            for (NSUInteger index = 0; index < groups; index++) {
                if (__builtin_add_overflow(total, parts[index], &total)) {
                    fail(@"C12 final partial reduction overflowed Int64");
                }
            }
            printf("RESULT\t%lld\n", total);
            fflush(stdout);
        }
    }
    return 0;
}
