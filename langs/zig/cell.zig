// The combd test cell in Zig (wasm32-wasi, no entry point).
const std = @import("std");
const wasi = std.os.wasi;

extern "combd" fn counter_incr() i64;

const gpa = std.heap.wasm_allocator;

export fn alloc(len: u32) [*]u8 {
    return (gpa.alloc(u8, @max(len, 1)) catch @trap()).ptr;
}

fn fib(n: u32) u64 {
    return if (n < 2) n else fib(n - 1) + fib(n - 2);
}

fn print(msg: []const u8) void {
    const iov = [_]wasi.ciovec_t{.{ .base = msg.ptr, .len = msg.len }};
    var written: usize = 0;
    _ = wasi.fd_write(1, &iov, iov.len, &written);
}

/// Returns the response as (ptr << 32) | len.
export fn handle(ptr: [*]u8, len: u32) u64 {
    const req = ptr[0..len];
    const buf = gpa.alloc(u8, 64) catch @trap();
    const out: []u8 = if (std.mem.eql(u8, req, "hello"))
        std.fmt.bufPrint(buf, "hello #{d}", .{counter_incr()}) catch @trap()
    else if (std.mem.startsWith(u8, req, "fib "))
        std.fmt.bufPrint(buf, "{d}", .{fib(std.fmt.parseInt(u32, req[4..], 10) catch 0)}) catch @trap()
    else if (std.mem.eql(u8, req, "print")) blk: {
        print("hi from zig\n");
        break :blk std.fmt.bufPrint(buf, "printed", .{}) catch @trap();
    } else if (std.mem.eql(u8, req, "spin")) {
        var x: u64 = 0;
        while (true) {
            x +%= 1;
            std.mem.doNotOptimizeAway(x);
        }
    } else if (std.mem.eql(u8, req, "panic"))
        @panic("boom")
    else
        std.fmt.bufPrint(buf, "unknown", .{}) catch @trap();
    return (@as(u64, @intFromPtr(out.ptr)) << 32) | out.len;
}
