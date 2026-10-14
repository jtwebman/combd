// The combd test cell in AssemblyScript.

@external("combd", "counter_incr")
declare function counter_incr(): i64;

export function alloc(len: u32): usize {
  return heap.alloc(max<u32>(len, 1));
}

function fib(n: u32): u64 {
  return n < 2 ? <u64>n : fib(n - 1) + fib(n - 2);
}

function respond(out: string): u64 {
  const buf = String.UTF8.encode(out);
  __pin(changetype<usize>(buf));
  return (<u64>changetype<usize>(buf) << 32) | <u64>buf.byteLength;
}

/** The host calls this after reading an answer. */
export function release(ptr: usize, len: u32): void {
  __unpin(ptr);
}

/** Returns the response as (ptr << 32) | len. */
export function handle(ptr: usize, len: u32): u64 {
  const req = String.UTF8.decodeUnsafe(ptr, len);
  heap.free(ptr);
  if (req == "hello") return respond("hello #" + counter_incr().toString());
  if (req.startsWith("fib ")) return respond(fib(<u32>I32.parseInt(req.substring(4))).toString());
  // AssemblyScript has no stdout of its own, so "print" just answers.
  if (req == "print") return respond("printed");
  if (req == "spin") {
    let x: u64 = 0;
    while (true) x++;
  }
  if (req == "panic") throw new Error("boom");
  return respond("unknown");
}
