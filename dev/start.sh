#!/bin/sh
# Start an inner dockerd, then idle so `docker exec` can build and run Tugport.
dockerd >/var/log/dockerd.log 2>&1 &
exec sleep infinity
