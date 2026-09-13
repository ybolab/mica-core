/* Compile-only Linux UAPI proof. No device is opened by this fixture. */
#include <stddef.h>
#include <linux/loop.h>
#include <linux/watchdog.h>
#include <sys/ioctl.h>
_Static_assert(sizeof(struct watchdog_info) == 40, "watchdog size");
_Static_assert(_Alignof(struct watchdog_info) == 4, "watchdog alignment");
_Static_assert(offsetof(struct watchdog_info, options) == 0, "watchdog options");
_Static_assert(offsetof(struct watchdog_info, firmware_version) == 4, "watchdog firmware");
_Static_assert(offsetof(struct watchdog_info, identity) == 8, "watchdog identity");
_Static_assert(sizeof(struct loop_info64) == 232, "loop size");
_Static_assert(_Alignof(struct loop_info64) == 8, "loop alignment");
_Static_assert(offsetof(struct loop_info64, lo_device) == 0, "loop device");
_Static_assert(offsetof(struct loop_info64, lo_inode) == 8, "loop inode");
_Static_assert(offsetof(struct loop_info64, lo_rdevice) == 16, "loop rdevice");
_Static_assert(offsetof(struct loop_info64, lo_offset) == 24, "loop offset");
_Static_assert(offsetof(struct loop_info64, lo_sizelimit) == 32, "loop limit");
_Static_assert(offsetof(struct loop_info64, lo_number) == 40, "loop number");
_Static_assert(offsetof(struct loop_info64, lo_encrypt_type) == 44, "loop encryption");
_Static_assert(offsetof(struct loop_info64, lo_encrypt_key_size) == 48, "loop key size");
_Static_assert(offsetof(struct loop_info64, lo_flags) == 52, "loop flags");
_Static_assert(offsetof(struct loop_info64, lo_file_name) == 56, "loop filename");
_Static_assert(offsetof(struct loop_info64, lo_crypt_name) == 120, "loop crypt name");
_Static_assert(offsetof(struct loop_info64, lo_encrypt_key) == 184, "loop key");
_Static_assert(offsetof(struct loop_info64, lo_init) == 216, "loop init");
_Static_assert(WDIOC_GETSUPPORT == 0x80285700, "watchdog support request");
_Static_assert(WDIOC_GETTIMEOUT == 0x80045707, "watchdog timeout request");
_Static_assert(WDIOC_KEEPALIVE == 0x80045705, "watchdog keepalive request");
_Static_assert(LOOP_GET_STATUS64 == 0x4c05, "loop status request");
_Static_assert(LOOP_CLR_FD == 0x4c01, "loop detach request");
_Static_assert(LOOP_CTL_GET_FREE == 0x4c82, "loop free request");
_Static_assert(LOOP_SET_FD == 0x4c00, "loop attach request");
_Static_assert(LOOP_SET_STATUS64 == 0x4c04, "loop configuration request");
_Static_assert(LO_FLAGS_READ_ONLY == 1, "loop read-only flag");
#include <linux/dm-ioctl.h>
_Static_assert(sizeof(struct dm_ioctl) == 312, "DM header size");
_Static_assert(_Alignof(struct dm_ioctl) == 8, "DM header alignment");
_Static_assert(offsetof(struct dm_ioctl, version) == 0, "DM version");
_Static_assert(offsetof(struct dm_ioctl, data_size) == 12, "DM response length");
_Static_assert(offsetof(struct dm_ioctl, data_start) == 16, "DM data offset");
_Static_assert(offsetof(struct dm_ioctl, target_count) == 20, "DM count");
_Static_assert(offsetof(struct dm_ioctl, open_count) == 24, "DM users");
_Static_assert(offsetof(struct dm_ioctl, flags) == 28, "DM flags");
_Static_assert(offsetof(struct dm_ioctl, event_nr) == 32, "DM generation event");
_Static_assert(offsetof(struct dm_ioctl, dev) == 40, "DM device");
_Static_assert(offsetof(struct dm_ioctl, name) == 48, "DM name");
_Static_assert(offsetof(struct dm_ioctl, uuid) == 176, "DM UUID");
_Static_assert(offsetof(struct dm_ioctl, data) == 305, "DM minimum reply");
_Static_assert(sizeof(struct dm_target_spec) == 40, "DM target size");
_Static_assert(_Alignof(struct dm_target_spec) == 8, "DM target alignment");
_Static_assert(offsetof(struct dm_target_spec, sector_start) == 0, "DM target sector");
_Static_assert(offsetof(struct dm_target_spec, length) == 8, "DM target length");
_Static_assert(offsetof(struct dm_target_spec, status) == 16, "DM target status");
_Static_assert(offsetof(struct dm_target_spec, next) == 20, "DM target absolute next");
_Static_assert(offsetof(struct dm_target_spec, target_type) == 24, "DM target type");
_Static_assert(DM_DEV_STATUS == 0xc138fd07, "DM status request");
_Static_assert(DM_TABLE_STATUS == 0xc138fd0c, "DM table request");
_Static_assert(DM_DEV_REMOVE == 0xc138fd04, "DM remove request");
_Static_assert(DM_STATUS_TABLE_FLAG == (1 << 4), "DM table flags");
_Static_assert(DM_DEV_CREATE == 0xc138fd03, "DM create request");
_Static_assert(DM_TABLE_LOAD == 0xc138fd09, "DM load request");
_Static_assert(DM_DEV_SUSPEND == 0xc138fd06, "DM resume request");
_Static_assert(DM_READONLY_FLAG == 1, "DM read-only flag");
