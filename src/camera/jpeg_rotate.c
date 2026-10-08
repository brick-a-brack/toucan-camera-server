/* Lossless (DCT-domain) JPEG rotation, driven from C.
 *
 * Why C rather than the libjpeg FFI directly from Rust: libjpeg reports a fatal
 * error by calling `error_exit`, which must never return. The only way to leave
 * it without returning is `longjmp`, which Rust cannot do. Panicking instead
 * looks like it works and does not: the unwind crosses libjpeg's frames with its
 * memory pools half-built, and the `jpeg_destroy_*` that follows corrupts the
 * heap — the process then dies in whatever allocates next, far from the cause.
 * (Observed on Android: a JERR_BAD_VIRTUAL_ACCESS during the transform, then a
 * SIGSEGV inside an unrelated JPEG decode one call later.)
 *
 * So the whole pipeline lives here behind one `setjmp`, and failures come back
 * as a plain return code.
 */

#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "jpeglib.h"
#include "transupp.h"

#include "jpeg_rotate.h"

/* Error manager that jumps instead of exiting. */
struct rotate_error_mgr {
    struct jpeg_error_mgr base;
    jmp_buf              escape;
    int                  code;
};

static void rotate_error_exit(j_common_ptr cinfo) {
    struct rotate_error_mgr *err = (struct rotate_error_mgr *)cinfo->err;
    err->code = cinfo->err->msg_code;
    longjmp(err->escape, 1);
}

/* libjpeg warns about recoverable oddities; nothing useful to do with those
 * here, and stderr goes nowhere on Android. */
static void rotate_silence(j_common_ptr cinfo) { (void)cinfo; }

int tc_rotate_jpeg(const unsigned char *in, size_t in_len, int degrees,
                   unsigned char **out, size_t *out_len, int *error_code) {
    JXFORM_CODE transform;
    struct jpeg_decompress_struct src;
    struct jpeg_compress_struct   dst;
    struct rotate_error_mgr       err;
    jpeg_transform_info           info;
    jvirt_barray_ptr             *src_coefs;
    jvirt_barray_ptr             *dst_coefs;
    unsigned char                *buffer = NULL;
    unsigned long                 buffer_len = 0;
    int                           result = TC_ROTATE_FAILED;

    if (!in || in_len == 0 || !out || !out_len) return TC_ROTATE_BAD_ARGUMENT;
    *out = NULL;
    *out_len = 0;
    if (error_code) *error_code = 0;

    switch (degrees) {
        case 90:  transform = JXFORM_ROT_90;  break;
        case 180: transform = JXFORM_ROT_180; break;
        case 270: transform = JXFORM_ROT_270; break;
        default:  return TC_ROTATE_BAD_ARGUMENT;
    }

    memset(&src,  0, sizeof(src));
    memset(&dst,  0, sizeof(dst));
    memset(&info, 0, sizeof(info));

    jpeg_std_error(&err.base);
    err.base.error_exit     = rotate_error_exit;
    err.base.output_message = rotate_silence;
    err.code                = 0;

    src.err = &err.base;
    dst.err = &err.base;

    if (setjmp(err.escape)) {
        /* Any libjpeg error lands here with both objects still destroyable. */
        if (error_code) *error_code = err.code;
        result = TC_ROTATE_FAILED;
        goto cleanup;
    }

    jpeg_create_decompress(&src);
    jpeg_create_compress(&dst);

    jpeg_mem_src(&src, in, (unsigned long)in_len);
    /* Carry EXIF, ICC and comments over; the caller resets the orientation tag,
     * since the pixels now hold that information. */
    jcopy_markers_setup(&src, JCOPYOPT_ALL);
    jpeg_read_header(&src, TRUE);

    info.transform = transform;
    /* Demand an exact transformation and never trim: dropping the partial edge
     * MCUs would silently shrink the image, by a different amount per angle.
     * When the dimensions are not a whole number of MCUs libjpeg refuses here
     * and the caller re-encodes the full image instead. */
    info.perfect = TRUE;
    info.trim    = FALSE;

    if (!jtransform_request_workspace(&src, &info)) {
        result = TC_ROTATE_NOT_EXACT;
        goto cleanup;
    }

    /* Requesting the workspace before this call is what gets it realized. */
    src_coefs = jpeg_read_coefficients(&src);

    /* Behave like plain libjpeg, not like mozjpeg: jpeg_copy_critical_parameters
     * calls jpeg_set_defaults internally, and mozjpeg's defaults turn a rotation
     * into a full recompression — progressive output plus Huffman optimisation,
     * ~1.5 s instead of ~100 ms on a 12 Mpx still, and a baseline camera JPEG
     * silently becoming progressive. Must be set before those defaults apply. */
    jpeg_c_set_int_param(&dst, JINT_COMPRESS_PROFILE, JCP_FASTEST);
    jpeg_copy_critical_parameters(&src, &dst);

    dst_coefs = jtransform_adjust_parameters(&src, &dst, src_coefs, &info);

    jpeg_mem_dest(&dst, &buffer, &buffer_len);
    jpeg_write_coefficients(&dst, dst_coefs);
    jcopy_markers_execute(&src, &dst, JCOPYOPT_ALL);
    jtransform_execute_transform(&src, &dst, src_coefs, &info);
    jpeg_finish_compress(&dst);

    if (buffer && buffer_len > 0) {
        *out     = buffer;
        *out_len = (size_t)buffer_len;
        buffer   = NULL; /* handed to the caller; tc_free_jpeg releases it */
        result   = TC_ROTATE_OK;
    }

cleanup:
    jpeg_destroy_compress(&dst);
    jpeg_destroy_decompress(&src);
    if (buffer) free(buffer);
    return result;
}

int tc_encode_rotated_rgb(const unsigned char *rgb, int width, int height,
                          int degrees, int quality,
                          unsigned char **out, size_t *out_len,
                          int *error_code) {
    struct jpeg_compress_struct dst;
    struct rotate_error_mgr     err;
    unsigned char              *buffer = NULL;
    unsigned long               buffer_len = 0;
    unsigned char              *row = NULL;
    int                         out_width, out_height;
    int                         result = TC_ROTATE_FAILED;
    int                         y;

    if (!rgb || width <= 0 || height <= 0 || !out || !out_len)
        return TC_ROTATE_BAD_ARGUMENT;
    *out = NULL;
    *out_len = 0;
    if (error_code) *error_code = 0;

    if (degrees != 0 && degrees != 90 && degrees != 180 && degrees != 270)
        return TC_ROTATE_BAD_ARGUMENT;

    if (degrees == 90 || degrees == 270) {
        out_width  = height;
        out_height = width;
    } else {
        out_width  = width;
        out_height = height;
    }

    row = (unsigned char *)malloc((size_t)out_width * 3);
    if (!row) return TC_ROTATE_FAILED;

    memset(&dst, 0, sizeof(dst));
    jpeg_std_error(&err.base);
    err.base.error_exit     = rotate_error_exit;
    err.base.output_message = rotate_silence;
    err.code                = 0;
    dst.err = &err.base;

    if (setjmp(err.escape)) {
        if (error_code) *error_code = err.code;
        result = TC_ROTATE_FAILED;
        goto cleanup;
    }

    jpeg_create_compress(&dst);
    /* Plain libjpeg behaviour: baseline, single pass. mozjpeg's defaults would
     * spend far longer chasing a smaller file than this path can afford. */
    jpeg_c_set_int_param(&dst, JINT_COMPRESS_PROFILE, JCP_FASTEST);
    dst.image_width      = (JDIMENSION)out_width;
    dst.image_height     = (JDIMENSION)out_height;
    dst.input_components = 3;
    dst.in_color_space   = JCS_RGB;
    jpeg_set_defaults(&dst);
    jpeg_set_quality(&dst, quality, TRUE);

    jpeg_mem_dest(&dst, &buffer, &buffer_len);
    jpeg_start_compress(&dst, TRUE);

    for (y = 0; y < out_height; y++) {
        int x;
        /* Gather one destination row out of the source. Which way it walks
         * depends on the angle: a half turn reads a row backwards, a quarter
         * turn reads down a column. */
        switch (degrees) {
            case 90:
                for (x = 0; x < out_width; x++) {
                    const unsigned char *px = rgb + ((size_t)(height - 1 - x) * width + y) * 3;
                    row[x * 3]     = px[0];
                    row[x * 3 + 1] = px[1];
                    row[x * 3 + 2] = px[2];
                }
                break;
            case 180:
                for (x = 0; x < out_width; x++) {
                    const unsigned char *px =
                        rgb + ((size_t)(height - 1 - y) * width + (width - 1 - x)) * 3;
                    row[x * 3]     = px[0];
                    row[x * 3 + 1] = px[1];
                    row[x * 3 + 2] = px[2];
                }
                break;
            case 270:
                for (x = 0; x < out_width; x++) {
                    const unsigned char *px = rgb + ((size_t)x * width + (width - 1 - y)) * 3;
                    row[x * 3]     = px[0];
                    row[x * 3 + 1] = px[1];
                    row[x * 3 + 2] = px[2];
                }
                break;
            default:
                memcpy(row, rgb + (size_t)y * width * 3, (size_t)out_width * 3);
                break;
        }
        {
            JSAMPROW rows[1];
            rows[0] = row;
            jpeg_write_scanlines(&dst, rows, 1);
        }
    }

    jpeg_finish_compress(&dst);

    if (buffer && buffer_len > 0) {
        *out     = buffer;
        *out_len = (size_t)buffer_len;
        buffer   = NULL;
        result   = TC_ROTATE_OK;
    }

cleanup:
    jpeg_destroy_compress(&dst);
    if (buffer) free(buffer);
    free(row);
    return result;
}

void tc_free_jpeg(unsigned char *data) {
    /* jpeg_mem_dest hands over a malloc'd buffer, so it is freed here rather
     * than through a Rust allocator that never owned it. */
    free(data);
}
