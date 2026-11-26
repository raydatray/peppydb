/*
 * The program that toolchains//:zig-runtime-cache-* links so that Zig builds
 * its runtime support (glibc start files and stubs, libunwind, compiler-rt)
 * into a cache that every later link starts from. Only the cache is kept.
 */
int main(void) {
    return 0;
}
