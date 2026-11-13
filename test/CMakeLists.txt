configure_file(testdir.h.in ${CMAKE_CURRENT_BINARY_DIR}/testdir.h @ONLY)
add_executable(testvoxkit testvoxkit.cpp)
target_include_directories(testvoxkit PRIVATE ${CMAKE_CURRENT_BINARY_DIR} ${PROJECT_SOURCE_DIR}/module)
target_link_libraries(testvoxkit Fcitx5::Core Fcitx5::Module::TestFrontend)
add_dependencies(testvoxkit voxkit)
# addon/voxkit.conf next to the test data dir
add_custom_command(TARGET testvoxkit POST_BUILD
  COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_CURRENT_BINARY_DIR}/addon
  COMMAND ${CMAKE_COMMAND} -E copy ${PROJECT_BINARY_DIR}/module/voxkit.conf ${CMAKE_CURRENT_BINARY_DIR}/addon/voxkit.conf)
add_test(NAME voxkit COMMAND ${CMAKE_CURRENT_SOURCE_DIR}/run-test.sh ${PROJECT_BINARY_DIR})
