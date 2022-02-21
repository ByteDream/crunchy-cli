package crunchyroll

import (
	"context"
	"crypto/aes"
	"crypto/cipher"
	"fmt"
	"github.com/grafov/m3u8"
	"io/ioutil"
	"math"
	"net/http"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"time"
)

type Downloader struct {
	// Filename is the filename of the output file
	Filename string
	// TempDir is the directory where the temporary files should be stored
	TempDir string
	// If IgnoreExisting is true, existing Filename's and TempDir's may be
	// overwritten or deleted
	IgnoreExisting bool
	// If DeleteTempAfter is true, the temp directory gets deleted afterwards.
	// Note that in case of a hard signal exit (os.Interrupt, ...) the directory
	// will NOT be deleted. In such situations try to catch the signal and
	// cancel Context
	DeleteTempAfter bool

	// Context to control the download process with.
	// There is a tiny delay when canceling the context and the actual stop of the
	// process. So it is not recommend stopping the program immediately after calling
	// the cancel function. It's better when canceling it and then exit the program
	// when Format.Download throws an error. See the signal handling section in
	// cmd/crunchyroll-go/cmd/download.go for an example
	Context context.Context

	// Goroutines is the number of goroutines to download segments with
	Goroutines int

	// A method to call when a segment was downloaded
	OnSegmentDownload func(segment *m3u8.MediaSegment, current, total int, file *os.File) error
	// If LockOnSegmentDownload is true, only one OnSegmentDownload function can be called at
	// once. Normally (because of the use of goroutines while downloading) multiple could get
	// called simultaneously
	LockOnSegmentDownload bool

	// If FFmpegOpts is not nil, ffmpeg will be used to merge and convert files.
	// The given opts will be used as ffmpeg parameters while merging.
	// Some opts are already used, see mergeSegmentsFFmpeg in format.go for more details
	FFmpegOpts []string
}

// NewDownloader creates a downloader with default settings which should
// fit the most needs
func NewDownloader(context context.Context, filename string, goroutines int, onSegmentDownload func(segment *m3u8.MediaSegment, current, total int, file *os.File) error) Downloader {
	tmp, _ := os.MkdirTemp("", "crunchy_")

	return Downloader{
		Filename:          filename,
		TempDir:           tmp,
		DeleteTempAfter:   true,
		Context:           context,
		Goroutines:        goroutines,
		OnSegmentDownload: onSegmentDownload,
	}
}

// download downloads every mpeg transport stream segment to a given directory (more information below).
// After every segment download onSegmentDownload will be called with:
//		the downloaded segment, the current position, the total size of segments to download, the file where the segment content was written to an error (if occurred).
// The filename is always <number of downloaded segment>.ts
//
// Short explanation:
// 		The actual crunchyroll video is split up in multiple segments (or video files) which have to be downloaded and merged after to generate a single video file.
//		And this function just downloads each of this segment into the given directory.
// 		See https://en.wikipedia.org/wiki/MPEG_transport_stream for more information
func download(context context.Context, format *Format, tempDir string, goroutines int, lockOnSegmentDownload bool, onSegmentDownload func(segment *m3u8.MediaSegment, current, total int, file *os.File) error) error {
	if err := format.InitVideo(); err != nil {
		return err
	}

	var wg sync.WaitGroup
	var lock sync.Mutex
	chunkSize := int(math.Ceil(float64(format.Video.Chunklist.Count()) / float64(goroutines)))

	// when a onSegmentDownload call returns an error, this channel will be set to true and stop all goroutines
	quit := make(chan bool)

	// receives the decrypt block and iv from the first segment.
	// in my tests, only the first segment has specified this data, so the decryption data from this first segments will be used in every other segment too
	block, iv, err := getCrypt(format, format.Video.Chunklist.Segments[0])
	if err != nil {
		return err
	}

	var total int32
	for i := 0; i < int(format.Video.Chunklist.Count()); i += chunkSize {
		wg.Add(1)
		end := i + chunkSize
		if end > int(format.Video.Chunklist.Count()) {
			end = int(format.Video.Chunklist.Count())
		}
		i := i

		go func() {
			defer wg.Done()

			for j, segment := range format.Video.Chunklist.Segments[i:end] {
				select {
				case <-context.Done():
					return
				case <-quit:
					return
				default:
					var file *os.File
					k := 1
					for ; k < 4; k++ {
						file, err = downloadSegment(context, format, segment, filepath.Join(tempDir, fmt.Sprintf("%d.ts", i+j)), block, iv)
						if err == nil {
							break
						}
						// sleep if an error occurs. very useful because sometimes the connection times out
						time.Sleep(5 * time.Duration(k) * time.Second)
					}
					if k == 4 {
						quit <- true
						return
					}
					if onSegmentDownload != nil {
						if lockOnSegmentDownload {
							lock.Lock()
						}

						if err = onSegmentDownload(segment, int(atomic.AddInt32(&total, 1)), int(format.Video.Chunklist.Count()), file); err != nil {
							quit <- true
							if lockOnSegmentDownload {
								lock.Unlock()
							}
							file.Close()
							return
						}
						if lockOnSegmentDownload {
							lock.Unlock()
						}
					}
					file.Close()
				}
			}
		}()
	}
	wg.Wait()

	select {
	case <-context.Done():
		return context.Err()
	case <-quit:
		return err
	default:
		return nil
	}
}

// getCrypt extracts the key and iv of a m3u8 segment and converts it into a cipher.Block block and a iv byte sequence
func getCrypt(format *Format, segment *m3u8.MediaSegment) (block cipher.Block, iv []byte, err error) {
	var resp *http.Response

	resp, err = format.crunchy.Client.Get(segment.Key.URI)
	if err != nil {
		return nil, nil, err
	}
	defer resp.Body.Close()
	key, err := ioutil.ReadAll(resp.Body)

	block, err = aes.NewCipher(key)
	if err != nil {
		return nil, nil, err
	}
	iv = []byte(segment.Key.IV)
	if len(iv) == 0 {
		iv = key
	}

	return block, iv, nil
}

// downloadSegment downloads a segment, decrypts it and names it after the given index
func downloadSegment(context context.Context, format *Format, segment *m3u8.MediaSegment, filename string, block cipher.Block, iv []byte) (*os.File, error) {
	// every segment is aes-128 encrypted and has to be decrypted when downloaded
	content, err := decryptSegment(context, format.crunchy.Client, segment, block, iv)
	if err != nil {
		return nil, err
	}

	file, err := os.Create(filename)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	if _, err = file.Write(content); err != nil {
		return nil, err
	}

	return file, nil
}

// https://github.com/oopsguy/m3u8/blob/4150e93ec8f4f8718875a02973f5d792648ecb97/tool/crypt.go#L25
func decryptSegment(context context.Context, client *http.Client, segment *m3u8.MediaSegment, block cipher.Block, iv []byte) ([]byte, error) {
	req, err := http.NewRequest(http.MethodGet, segment.URI, nil)
	if err != nil {
		return nil, err
	}
	req.WithContext(context)

	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	raw, err := ioutil.ReadAll(resp.Body)
	if err != nil {
		return nil, err
	}

	blockMode := cipher.NewCBCDecrypter(block, iv[:block.BlockSize()])
	decrypted := make([]byte, len(raw))
	blockMode.CryptBlocks(decrypted, raw)
	raw = pkcs5UnPadding(decrypted)

	return raw, nil
}

// https://github.com/oopsguy/m3u8/blob/4150e93ec8f4f8718875a02973f5d792648ecb97/tool/crypt.go#L47
func pkcs5UnPadding(origData []byte) []byte {
	length := len(origData)
	unPadding := int(origData[length-1])
	return origData[:(length - unPadding)]
}