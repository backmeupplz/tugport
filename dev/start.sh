#!/bin/sh
# Start an inner dockerd, then idle so `docker exec` can build and run Tugport.
# On cgroup v2, first move this box's processes out of its root cgroup so dockerd can
# use every controller; without it, containers with memory limits fail ("threaded mode").
if [ -f /sys/fs/cgroup/cgroup.controllers ]; then
  mkdir -p /sys/fs/cgroup/init
  xargs -rn1 < /sys/fs/cgroup/cgroup.procs > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || :
  sed -e 's/ / +/g' -e 's/^/+/' < /sys/fs/cgroup/cgroup.controllers > /sys/fs/cgroup/cgroup.subtree_control
fi
# Pid files from before a `docker restart` would make dockerd wait for a containerd that is gone.
find /run /var/run \( -name 'docker*.pid' -o -name 'containerd*.pid' \) -delete 2>/dev/null || :
dockerd >/var/log/dockerd.log 2>&1 &
exec sleep infinity
