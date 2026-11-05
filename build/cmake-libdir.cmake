# CMake toolchain file for the C/C++ libraries built by cargo (CharLS, for JPEG-LS).
#
# charls-sys looks for the library in <out>/lib, but GNUInstallDirs installs it in lib64 on
# Fedora, RHEL and openSUSE: install to lib everywhere.
set(CMAKE_INSTALL_LIBDIR lib CACHE PATH "")
