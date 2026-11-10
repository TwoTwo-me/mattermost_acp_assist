#!/bin/sh
echo "STOP-HOOK $(date +%s)" >> /tmp/assist-hook.log
printf '%s\n' '{"systemMessage":"STOP-HOOK-7777"}'
