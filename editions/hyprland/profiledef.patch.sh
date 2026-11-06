# Appended after common/profiledef.patch.sh for this edition only.
file_permissions+=(
  ["/home/osmium"]="1000:1000:750"
  ["/home/osmium/.bash_profile"]="1000:1000:644"
)
