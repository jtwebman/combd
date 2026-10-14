// The combd test cell in Go (TinyGo, wasip1, reactor).
package main

import (
	"fmt"
	"strconv"
	"strings"
	"unsafe"
)

//go:wasmimport combd counter_incr
func counterIncr() int64

// Buffers handed to the host stay alive here until the cell is done with them.
var keep = map[uintptr][]byte{}

var spins uint64

func fib(n uint32) uint64 {
	if n < 2 {
		return uint64(n)
	}
	return fib(n-1) + fib(n-2)
}

//go:wasmexport alloc
func alloc(n uint32) uint32 {
	b := make([]byte, n+1)
	p := uintptr(unsafe.Pointer(&b[0]))
	keep[p] = b
	return uint32(p)
}

// release lets go of an answer once the host has read it.
//
//go:wasmexport release
func release(ptr, n uint32) {
	delete(keep, uintptr(ptr))
}

// handle returns the response as (ptr << 32) | len.
//
//go:wasmexport handle
func handle(ptr, n uint32) uint64 {
	req := string(unsafe.Slice((*byte)(unsafe.Pointer(uintptr(ptr))), n))
	delete(keep, uintptr(ptr))
	var out string
	switch {
	case req == "hello":
		out = fmt.Sprintf("hello #%d", counterIncr())
	case strings.HasPrefix(req, "fib "):
		k, _ := strconv.Atoi(req[4:])
		out = strconv.FormatUint(fib(uint32(k)), 10)
	case req == "print":
		fmt.Println("hi from go")
		out = "printed"
	case req == "spin":
		for {
			spins++
		}
	case req == "panic":
		panic("boom")
	default:
		out = "unknown"
	}
	b := []byte(out)
	p := uintptr(unsafe.Pointer(&b[0]))
	keep[p] = b
	return uint64(p)<<32 | uint64(len(b))
}

func main() {}
