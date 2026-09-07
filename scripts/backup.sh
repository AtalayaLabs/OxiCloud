#!/bin/bash

NOW="$(date '+%Y-%m-%d %H:%M:%S')"

pg_dump postgres://postgres:postgres@localhost:5432/oxicloud  -F c --disable-triggers > "backup.${NOW}.dump"
