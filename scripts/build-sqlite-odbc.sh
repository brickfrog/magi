#!/usr/bin/env bash
# Build Christian Werner's SQLite ODBC driver (http://www.ch-werner.de/sqliteodbc/) against the
# system sqlite3 and unixODBC headers, without installing anything system-wide.
#
# Usage: scripts/build-sqlite-odbc.sh [CACHE_DIR]
#   CACHE_DIR defaults to <repo>/target/odbc-driver.
#
# Prints the absolute path of the built driver on stdout (all progress goes to stderr), so it can
# be used as:
#   export MAGI_TEST_SQLITE_ODBC_DRIVER="$(scripts/build-sqlite-odbc.sh)"
#   # connection string: Driver=$MAGI_TEST_SQLITE_ODBC_DRIVER;Database=/tmp/x.db;
#
# Idempotent: when the driver has already been built in CACHE_DIR it is not rebuilt.
# Requires: curl or wget, tar, a C compiler, make, /usr/include/sqlite3.h, /usr/include/sql.h.
set -euo pipefail

VERSION="0.99991"
SHA256="4d94adb8d3cde1fa94a28aeb0dfcc7be73145bcdfcdf3d5e225434db31dc8a5c"
TARBALL="sqliteodbc-${VERSION}.tar.gz"
URL="http://www.ch-werner.de/sqliteodbc/${TARBALL}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cache_dir="${1:-${repo_root}/target/odbc-driver}"
mkdir -p "${cache_dir}"
cache_dir="$(cd "${cache_dir}" && pwd)"
driver="${cache_dir}/libsqlite3odbc.so"

log() { printf '%s\n' "$*" >&2; }

if [[ -f "${driver}" ]]; then
    printf '%s\n' "${driver}"
    exit 0
fi

for header in /usr/include/sqlite3.h /usr/include/sql.h; do
    [[ -f "${header}" ]] || { log "error: missing ${header} (install sqlite3 / unixODBC development headers)"; exit 1; }
done

archive="${cache_dir}/${TARBALL}"
if [[ ! -f "${archive}" ]] || ! echo "${SHA256}  ${archive}" | sha256sum --check --status; then
    log "downloading ${URL}"
    tmp="${archive}.part"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "${tmp}" "${URL}"
    else
        wget -q -O "${tmp}" "${URL}"
    fi
    if ! echo "${SHA256}  ${tmp}" | sha256sum --check --status; then
        rm -f "${tmp}"
        log "error: checksum mismatch for ${TARBALL}"
        exit 1
    fi
    mv "${tmp}" "${archive}"
fi

src="${cache_dir}/sqliteodbc-${VERSION}"
rm -rf "${src}"
tar -xzf "${archive}" -C "${cache_dir}"

log "building sqliteodbc ${VERSION} in ${src}"
(
    cd "${src}"
    # The sources use K&R-style empty parameter lists for function pointers, which C23 (the GCC 15
    # default) rejects; build as gnu17.
    CFLAGS="-O2 -std=gnu17" ./configure --with-sqlite3=/usr --with-odbc=/usr >build.log 2>&1 \
        || { log "error: configure failed; see ${src}/build.log"; exit 1; }
    make libsqlite3odbc.la >>build.log 2>&1 \
        || { log "error: make failed; see ${src}/build.log"; exit 1; }
)

built="${src}/.libs/libsqlite3odbc.so"
[[ -f "${built}" ]] || { log "error: ${built} was not produced"; exit 1; }
# Copy via a temp name so an interrupted run never leaves a half-written driver behind.
cp -L "${built}" "${driver}.part"
mv "${driver}.part" "${driver}"
printf '%s\n' "${driver}"
