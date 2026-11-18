"""Bound tar metadata before Python allocates extension-header contents."""

import tarfile


class BoundedTarInfo(tarfile.TarInfo):
    """Limit extension headers before tarfile allocates their decoded contents."""

    def _proc_pax(self, archive):
        if not 0 <= self.size <= 65536:
            raise ValueError("Oversized archive extension header")
        return super()._proc_pax(archive)

    def _proc_gnulong(self, archive):
        if not 0 <= self.size <= 65536:
            raise ValueError("Oversized archive extension header")
        return super()._proc_gnulong(archive)

    def _reject_sparse(self, *args):
        raise ValueError("Sparse candidate archive members are unsupported")

    _proc_sparse = _reject_sparse
    _proc_gnusparse_00 = _reject_sparse
    _proc_gnusparse_01 = _reject_sparse
    _proc_gnusparse_10 = _reject_sparse
