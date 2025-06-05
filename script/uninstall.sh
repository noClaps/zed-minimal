#!/usr/bin/env sh
set -eu

# Uninstalls Zed that was installed using the install.sh script

check_remaining_installations() {
    platform="$(uname -s)"
    if [ "$platform" = "Darwin" ]; then
        # Check for any Zed variants in /Applications
        remaining=$(ls -d /Applications/Zed*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    else
        # Check for any Zed variants in ~/.local
        remaining=$(ls -d "$HOME/.local/zed"*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    fi
}

prompt_remove_preferences() {
    printf "Do you want to keep your Zed preferences? [Y/n] "
    read -r response
    case "$response" in
        [nN]|[nN][oO])
            rm -rf "$HOME/.config/zed"
            echo "Preferences removed."
            ;;
        *)
            echo "Preferences kept."
            ;;
    esac
}

main() {
    channel="${ZED_CHANNEL:-stable}"
    platform="macos"

    "$platform"

    echo "Zed has been uninstalled"
}

macos() {
    app="ZedMin.app"
    db_suffix="stable"
    app_id="dev.zerolimits.ZedMin"

    # Remove the app bundle
    if [ -d "/Applications/$app" ]; then
        rm -rf "/Applications/$app"
    fi

    # Remove the binary symlink
    rm -f "$HOME/.local/bin/zed-min"

    # Remove the database directory for this channel
    rm -rf "$HOME/Library/Application Support/ZedMin/db/0-$db_suffix"

    # Remove app-specific files and directories
    rm -rf "$HOME/Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments/$app_id.sfl"*
    rm -rf "$HOME/Library/Caches/$app_id"
    rm -rf "$HOME/Library/HTTPStorages/$app_id"
    rm -rf "$HOME/Library/Preferences/$app_id.plist"
    rm -rf "$HOME/Library/Saved Application State/$app_id.savedState"

    # Remove the entire Zed directory if no installations remain
    if check_remaining_installations; then
        rm -rf "$HOME/Library/Application Support/ZedMin"
        rm -rf "$HOME/Library/Logs/ZedMin"

        prompt_remove_preferences
    fi

    rm -rf $HOME/.zed_min_server
}

main "$@"
