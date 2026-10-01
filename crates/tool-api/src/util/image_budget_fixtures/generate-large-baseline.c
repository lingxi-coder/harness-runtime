#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <jpeglib.h>
int main(int argc, char **argv) {
  FILE *file = fopen(argc > 1 ? argv[1] : "large-baseline.jpg", "wb");
  if (!file) return 1;
  struct jpeg_compress_struct c;
  struct jpeg_error_mgr e;
  c.err = jpeg_std_error(&e);
  jpeg_create_compress(&c);
  jpeg_stdio_dest(&c, file);
  c.image_width = 16000;
  c.image_height = 12000;
  c.input_components = 3;
  c.in_color_space = JCS_RGB;
  jpeg_set_defaults(&c);
  jpeg_set_quality(&c, 80, TRUE);
  jpeg_start_compress(&c, TRUE);
  unsigned char row[16000 * 3];
  memset(row, 128, sizeof(row));
  JSAMPROW rows[1] = { row };
  while (c.next_scanline < c.image_height) jpeg_write_scanlines(&c, rows, 1);
  jpeg_finish_compress(&c);
  jpeg_destroy_compress(&c);
  fclose(file);
  return 0;
}
