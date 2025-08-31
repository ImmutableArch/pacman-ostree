cmake_minimum_required(VERSION 3.10)

# Nazwa projektu i język
project(pacman-ostree C)

# Ustawienie standardu C
set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED True)


# Specify the include directory
include_directories(
    /usr/include/ostree-1
    /usr/include/glib-2.0
    /usr/lib64/glib-2.0/include
    /usr/lib/glib-2.0/include
)

# Specify the library directories
link_directories(
    /usr/lib
    /usr/lib64
)

# Utworzenie pliku wykonywalnego z plików źródłowych
add_executable(pacman-ostree
    src/main.c
    src/ostree-managent/commit.c
    src/ostree-managent/init-repo.c
)

target_link_libraries(pacman-ostree ostree-1 glib-2.0)
