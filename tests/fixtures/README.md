`silence.m4a` is 0.25 seconds of generated mono silence. It exercises AAC/MP4
progressive parsing and seeking without network access or a test-time FFmpeg dependency.

Generation command:

```
ffmpeg -f lavfi -i anullsrc=r=48000:cl=mono -t 0.25 -c:a aac -b:a 128k -movflags +faststart silence.m4a
```
