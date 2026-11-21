### Downloads

| Platform | File |
|---|---|
| macOS (Apple Silicon) | `Nodal-{{VERSION}}-macos-arm64.dmg` |
| Linux x86_64 · arm64 | `nodal-{{TAG}}-x86_64-unknown-linux-gnu.tar.gz` · `nodal-{{TAG}}-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `nodal-{{TAG}}-x86_64-pc-windows-msvc.zip` |
| Docker / Unraid | `ghcr.io/au12/nodal:{{VERSION}}` (also `{{MINOR}}` and `latest`) |

`SHA256SUMS.txt` lists checksums for every file.

**macOS.** Open the disk image and drag Nodal to Applications. If macOS says it can't verify the app (builds without a Developer ID are ad-hoc signed), choose **Done**, then go to **System Settings → Privacy & Security → Open Anyway**. On macOS 14 and earlier, right-click the app and choose **Open**. Or run `xattr -dr com.apple.quarantine /Applications/Nodal.app`.

**Docker**

```sh
docker run -d --name nodal -p 8080:8080 -v /path/to/nodal-data:/data -e NODAL_TOKEN=change-me ghcr.io/au12/nodal:{{VERSION}}
```

**Unraid.** Install the template from the Unraid terminal, then go to Docker → Add Container → **Nodal**.

```sh
wget -O /boot/config/plugins/dockerMan/templates-user/my-Nodal.xml https://raw.githubusercontent.com/au12/Nodal/main/unraid/nodal.xml
```

The [README](https://github.com/au12/Nodal#readme) covers usage, and the [roadmap](https://github.com/au12/Nodal/blob/main/ROADMAP.md) covers what's next.
