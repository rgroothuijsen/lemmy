#!/bin/sh

# This script uses a docker file that builds with musl, and runs on linux alpine
# Its a bit slower for development than the volume mount.

set -e

mkdir -p volumes/pictrs
sudo chown -R 991:991 volumes/pictrs
sudo docker-compose up -d --build
