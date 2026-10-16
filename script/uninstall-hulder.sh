#!/usr/bin/env sh
set -eu

# Uninstalls Hulder. Only Hulder-owned paths are touched; nothing named Zed is
# removed, so a side-by-side Zed install is left alone.

confirm() {
    printf "%s [y/N] " "$1"
    read -r response
    case "$response" in
        [yY]|[yY][eE][sS]) return 0 ;;
        *) return 1 ;;
    esac
}

remove_path() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        rm -rf "$1"
        echo "Removed $1"
    fi
}

remove_glob() {
    for match in "$@"; do
        remove_path "$match"
    done
}

main() {
    platform="$(uname -s)"
    channel="${ZED_CHANNEL:-stable}"

    app="Hulder"
    app_id="io.hulder.Hulder"
    case "$channel" in
      nightly)
        app="Hulder Nightly"
        app_id="io.hulder.Hulder-Nightly"
        ;;
      preview)
        app="Hulder Preview"
        app_id="io.hulder.Hulder-Preview"
        ;;
      dev)
        app="Hulder Dev"
        app_id="io.hulder.Hulder-Dev"
        ;;
      stable) ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        ;;
    esac

    case "$platform" in
        Darwin) macos ;;
        Linux) linux ;;
        *)
            echo "Unsupported platform $platform"
            exit 1
            ;;
    esac

    echo "Hulder has been uninstalled"
}

remove_cli_links() {
    for link in /usr/local/bin/hulder "$HOME/.local/bin/hulder"; do
        if [ -L "$link" ] || [ -e "$link" ]; then
            if [ -w "$(dirname "$link")" ]; then
                remove_path "$link"
            else
                echo "Skipping $link (not writable; remove it with sudo if desired)"
            fi
        fi
    done
}

remove_user_data() {
    if confirm "Remove Hulder settings and data (${HOME}/.config/hulder and application support)?"; then
        remove_path "$HOME/.config/hulder"
        remove_path "$1"
        echo "Settings and data removed."
    else
        echo "Settings and data kept."
    fi
}

macos() {
    bundle="/Applications/$app.app"
    if [ -d "$bundle" ]; then
        if confirm "Remove $bundle?"; then
            remove_path "$bundle"
        else
            echo "Leaving $bundle in place."
        fi
    fi

    remove_cli_links

    remove_glob "$HOME/Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments/$app_id.sfl"*
    remove_path "$HOME/Library/Caches/$app_id"
    remove_path "$HOME/Library/HTTPStorages/$app_id"
    remove_path "$HOME/Library/Preferences/$app_id.plist"
    remove_path "$HOME/Library/Saved Application State/$app_id.savedState"
    remove_path "$HOME/Library/Logs/Hulder"

    remove_user_data "$HOME/Library/Application Support/Hulder"
}

linux() {
    suffix=""
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi

    remove_path "$HOME/.local/hulder$suffix.app"
    remove_cli_links
    remove_path "$HOME/.local/share/applications/${app_id}.desktop"

    data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
    remove_user_data "$data_home/hulder"
}

main "$@"
