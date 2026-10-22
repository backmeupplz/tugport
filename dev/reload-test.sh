#!/bin/sh
# Inside the dev box: reload the server twice under continuous load and count failures.
P='^/usr/local/bin/tugport server'
echo "pid before: $(pgrep -f "$P")"
rm -f /tmp/bad2.txt
( end=$(($(date +%s)+25)); ok=0; bad=0; while [ $(date +%s) -lt $end ]; do c=$(curl -s -m 5 -o /dev/null -w "%{http_code}" -H "Host: web.example.test" http://localhost/); if [ "$c" = 200 ]; then ok=$((ok+1)); else bad=$((bad+1)); echo "$c" >> /tmp/bad2.txt; fi; done; echo "proxy ok=$ok failed=$bad" > /tmp/load2.txt ) &
( end=$(($(date +%s)+25)); ok=0; bad=0; while [ $(date +%s) -lt $end ]; do c=$(curl -s -m 5 -o /dev/null -w "%{http_code}" http://localhost:8484/api/setup); if [ "$c" = 200 ]; then ok=$((ok+1)); else bad=$((bad+1)); fi; done; echo "panel ok=$ok failed=$bad" > /tmp/load3.txt ) &
sleep 5; kill -HUP "$(pgrep -f "$P")"; sleep 6; kill -HUP "$(pgrep -f "$P")"; wait
cat /tmp/load2.txt /tmp/load3.txt; sort /tmp/bad2.txt 2>/dev/null | uniq -c
echo "pid after: $(pgrep -f "$P")"; echo "handoffs: $(grep -c handing /var/log/tugport.log)"
