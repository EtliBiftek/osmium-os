# Sourced after releng profiledef.sh; overrides branding.
iso_name="osmiumos-${EDITION}"
iso_label="OSMIUM_$(date --date="@${SOURCE_DATE_EPOCH:-$(date +%s)}" +%Y%m)"
iso_publisher="OsmiumOS <https://github.com/EtliBiftek/osmium-os>"
iso_application="OsmiumOS ${EDITION} Live"
# zstd instead of releng's xz: the installer unpacks this image, and zstd decompresses far faster.
airootfs_image_tool_options=('-comp' 'zstd' '-Xcompression-level' '15' '-b' '1M')
file_permissions+=(
  ["/etc/sudoers.d/osmium"]="0:0:440"
)
