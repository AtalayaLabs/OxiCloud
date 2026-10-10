#!/bin/sh

for I in $(seq 0 499)
do
    TEXT=$(printf "%05d" $I)
    test -e img-$TEXT.png || magick -pointsize 72 label:"$TEXT" img-$TEXT.png
done

