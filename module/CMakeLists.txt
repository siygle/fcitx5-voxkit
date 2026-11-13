add_fcitx5_addon(voxkit voxkit.cpp VoiceClient.cpp)
target_link_libraries(voxkit Fcitx5::Core Fcitx5::Config PkgConfig::JSONC)
target_compile_definitions(voxkit PRIVATE FCITX_GETTEXT_DOMAIN=\"fcitx5-voxkit\")
set_target_properties(voxkit PROPERTIES PREFIX "")
install(TARGETS voxkit DESTINATION "${FCITX_INSTALL_ADDONDIR}")

configure_file(voxkit.conf.in voxkit.conf @ONLY)
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/voxkit.conf" DESTINATION "${FCITX_INSTALL_PKGDATADIR}/addon")

# Public header + CMake package so input methods can call registerInputMethod.
fcitx5_export_module(Voxkit TARGET voxkit BUILD_INCLUDE_DIRECTORIES "${CMAKE_CURRENT_SOURCE_DIR}"
                     HEADERS voxkit_public.h INSTALL)
