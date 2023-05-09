#! /bin/bash

COMMIT="${COMMIT:-master}"
PREFIX="${PREFIX:-$PWD/vendor/openssl}"
OPENSSLDIR="${OPENSSLDIR:-/usr/local/ssl}"

echo "prefix    : ${PREFIX}"
echo "openssldir: ${OPENSSLDIR}"

wget https://github.com/openssl/openssl/archive/${COMMIT}.tar.gz
tar -xf ${COMMIT}.tar.gz
rm ${COMMIT}.tar.gz
cd openssl-${COMMIT}
./Configure --prefix=${PREFIX} --openssldir=${OPENSSLDIR} '-Wl,--enable-new-dtags,-rpath,$(LIBRPATH)'
make -j $nproc
make install
cd ..
rm -r openssl-${COMMIT}
