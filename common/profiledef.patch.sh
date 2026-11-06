# Sourced after releng profiledef.sh; overrides branding.
iso_name="osmiumos-${EDITION}"
iso_label="OSMIUM_$(date --date="@${SOURCE_DATE_EPOCH:-$(date +%s)}" +%Y%m)"
iso_publisher="OsmiumOS <https://github.com/EtliBiftek/osmium-os>"
iso_application="OsmiumOS ${EDITION} Live"
file_permissions+=(
  ["/etc/sudoers.d/osmium"]="0:0:440"
  ["/home/osmium"]="1000:1000:750"
  ["/home/osmium/.bash_profile"]="1000:1000:644"
)
