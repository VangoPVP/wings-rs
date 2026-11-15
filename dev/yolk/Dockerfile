# Minimal test "yolk": runs $STARTUP from /home/container, like Pterodactyl yolks.
FROM busybox:latest
WORKDIR /home/container
CMD ["/bin/sh", "-c", "cd /home/container && eval \"$STARTUP\""]
