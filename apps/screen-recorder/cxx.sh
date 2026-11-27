#!/bin/sh
# The C++ compiler for this app's cross build, which tools/appimage/build.sh uses
# because this file is here.
#
# OpenH264's build script asks for -fstack-protector-all, and on aarch64 glibc keeps
# the canary in the dynamic loader, so the binary would need ld-linux as well as libc.
# The encoder only ever sees the panel's own fixed-size frames, so the flag is dropped
# here; cc puts it after CXXFLAGS, which is why an environment variable cannot.
for arg in "$@"; do
    shift
    [ "$arg" = -fstack-protector-all ] || set -- "$@" "$arg"
done
exec aarch64-linux-gnu-g++ "$@"
