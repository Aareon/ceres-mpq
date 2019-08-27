# 0.1.3

* `Creator.write()` now takes `self` by mutable reference instead of by value

# 0.1.2

* Fixed a bug where the sector size shift would be off-by-one, causing multi-sector files to be incorrectly read in other programs
* Fixed a bug where backwards slashes would be incorrectly converted to forward slashes in the listfile, instead of the other way around


# 0.1.1

* Removed some dead code from the library
* Added `.start()`, `.end()`, `.size()` and `.reader()` methods to `Archive` 

# 0.1.0

Initial release