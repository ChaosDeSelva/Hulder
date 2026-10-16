# LÖVE game tools

A reference Hulder API 1 feature add on. It adds a native game panel, a configurable runtime path, two commands and a playable project template.

Open Extensions, choose Feature add ons, then Install or update from folder and select this directory. Review the permissions and enable the add on. Set runtime to your LÖVE executable and save settings. The macOS default is /Applications/love.app/Contents/MacOS/love.

Choose Create LÖVE starter project and select a parent folder. A new love-starter folder is created without replacing existing files. Open that folder in Hulder, trust the project, save your files and choose Run game. The command approval shows the executable and project directory. The game runs in its own window; its output and process status are visible in the IDE terminal.

This add on does not install LÖVE or LuaLS and does not contain a debugger or visual scene editor. Install the Lua language extension separately. Full API completion requires suitable LÖVE type definitions configured with LuaLS.

Commands can also be bound using hulder_addons::Run with addon set to love and command set to run or version. They always pass through the same approval and project trust checks.
