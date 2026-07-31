package boxlite

/*
#include "bridge.h"
#include <stdlib.h>
*/
import "C"
import (
	"context"
	"runtime/cgo"
	"unsafe"
)

// Archive describes an exported BoxLite archive.
type Archive struct {
	Path           string
	SHA256         string
	SizeBytes      uint64
	ArchiveVersion uint32
}

type archiveResult struct {
	value Archive
	err   error
}

// ExportOptions controls the archive format produced by Box.Export.
type ExportOptions struct {
	// AsDirectory writes manifest.json plus content-addressed layer objects
	// instead of one .boxlite file.
	AsDirectory bool
}

// Export writes the box archive into dest and returns its path and integrity
// metadata.
func (b *Box) Export(ctx context.Context, dest string, opts ExportOptions) (Archive, error) {
	b.runtime.ensureDrainRunning()

	cDest := toCString(dest)
	defer C.free(unsafe.Pointer(cDest))

	ch := make(chan archiveResult, 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))

	cOpts := C.CArchiveExportOptions{as_directory: cBool(opts.AsDirectory)}
	var cerr C.CBoxliteError
	code := C.boxlite_box_export(b.handle, cDest, cOpts, C.cbExportBox(), handleToPtr(h), &cerr)
	if code != C.Ok {
		deleteHandleForDispatch(h)
		return Archive{}, freeError(&cerr)
	}

	select {
	case res := <-ch:
		return res.value, res.err
	case <-ctx.Done():
		drainAndDelete(ch, h, b.runtime.closing)
		return Archive{}, ctx.Err()
	case <-b.runtime.closing:
		drainAndDelete(ch, h, b.runtime.closing)
		return Archive{}, ErrRuntimeClosed
	}
}

// Import restores an archive into this runtime. If name is empty, the archive's
// recorded box name is used.
func (r *Runtime) Import(ctx context.Context, archivePath, name string) (*Box, error) {
	r.ensureDrainRunning()

	cArchive := toCString(archivePath)
	defer C.free(unsafe.Pointer(cArchive))
	var cName *C.char
	if name != "" {
		cName = toCString(name)
		defer C.free(unsafe.Pointer(cName))
	}

	ch := make(chan handleResult[*C.CBoxHandle], 1)
	h := registerHandleForDispatch(cgo.NewHandle(ch))

	var cerr C.CBoxliteError
	code := C.boxlite_runtime_import_box(r.handle, cArchive, cName, C.cbImportBox(), handleToPtr(h), &cerr)
	if code != C.Ok {
		deleteHandleForDispatch(h)
		return nil, freeError(&cerr)
	}

	freeOrphanHandle := func(handle *C.CBoxHandle) {
		if handle != nil {
			C.boxlite_box_free(handle)
		}
	}

	select {
	case res := <-ch:
		if res.err != nil {
			return nil, res.err
		}
		return newBoxFromHandle(r, res.value, name), nil
	case <-ctx.Done():
		abandonAsync(ch, h, r.closing, freeOrphanHandle)
		return nil, ctx.Err()
	case <-r.closing:
		abandonAsync(ch, h, r.closing, freeOrphanHandle)
		return nil, ErrRuntimeClosed
	}
}
