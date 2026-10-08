#pragma once
#include <stddef.h>

/* Return codes of tc_rotate_jpeg. */
#define TC_ROTATE_OK            0
/* The dimensions are not a whole number of MCUs, so no exact coefficient
 * permutation exists. The caller re-encodes instead; nothing is ever cropped. */
#define TC_ROTATE_NOT_EXACT     1
/* libjpeg reported a fatal error; *error_code carries its J_MESSAGE_CODE. */
#define TC_ROTATE_FAILED        2
#define TC_ROTATE_BAD_ARGUMENT  3

/* Rotates a JPEG clockwise by 90, 180 or 270 degrees in the DCT domain: the
 * entropy-coded coefficients are read, the blocks permuted and re-encoded. No
 * IDCT/DCT, no colour conversion, no requantisation — the pixels come back
 * identical, turned.
 *
 * On TC_ROTATE_OK the caller owns *out and must release it with tc_free_jpeg.
 */
int tc_rotate_jpeg(const unsigned char *in, size_t in_len, int degrees,
                   unsigned char **out, size_t *out_len, int *error_code);

/* Encodes a packed RGB24 buffer as a baseline JPEG, rotating it clockwise by 0,
 * 90, 180 or 270 degrees on the way.
 *
 * The fallback for images the lossless transform cannot handle exactly. The
 * rotation costs nothing on top of the encode: libjpeg consumes the image one
 * scanline at a time, so each destination row is gathered straight from the
 * source instead of materialising a second full-size buffer.
 *
 * `width` and `height` describe the *source*; a quarter turn swaps them in the
 * output. On TC_ROTATE_OK the caller owns *out and must release it with
 * tc_free_jpeg.
 */
int tc_encode_rotated_rgb(const unsigned char *rgb, int width, int height,
                          int degrees, int quality,
                          unsigned char **out, size_t *out_len,
                          int *error_code);

/* Releases a buffer returned by tc_rotate_jpeg or tc_encode_rotated_rgb. */
void tc_free_jpeg(unsigned char *data);
