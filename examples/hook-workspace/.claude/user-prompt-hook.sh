#!/bin/sh
echo "USER-PROMPT-HOOK $(date +%s)" >> /tmp/assist-hook.log
printf '%s\n' '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"HOOK-CONTEXT-4242: this text came from the folder hook"}}'
