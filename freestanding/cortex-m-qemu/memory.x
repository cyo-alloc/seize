/* The MPS2-AN386 FPGA image: code in the ZBT SSRAM at 0, data in the SSRAM
   at 0x20000000. */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 4M
  RAM   : ORIGIN = 0x20000000, LENGTH = 4M
}
